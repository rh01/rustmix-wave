"""Day-level FSRS-6 used to cross-check the firmware scheduler.

Formulas follow py-fsrs (MIT) `Scheduler` with empty learning_steps,
empty relearning_steps, and fuzzing disabled. The device has no minute-level
learning steps: every scheduled interval is a whole day, at least 1 and at
most 36500. Same-day reviews (elapsed days < 1) use short-term stability.
"""

from __future__ import annotations

import math
from dataclasses import dataclass

DEFAULT_PARAMETERS = (
    0.212,
    1.2931,
    2.3065,
    8.2956,
    6.4133,
    0.8334,
    3.0194,
    0.001,
    1.8722,
    0.1666,
    0.796,
    1.4835,
    0.0614,
    0.2629,
    1.6483,
    0.6014,
    1.8729,
    0.5425,
    0.0912,
    0.0658,
    0.1542,
)

STABILITY_MIN = 0.001
RATING = {"Again": 1, "Hard": 2, "Good": 3, "Easy": 4}


@dataclass
class Card:
    state: str = "New"
    stability: float | None = None
    difficulty: float | None = None
    last_day: int | None = None
    due_day: int = 0
    interval: int = 0


class Scheduler:
    def __init__(self, desired_retention: float = 0.9, parameters=DEFAULT_PARAMETERS):
        self.w = tuple(parameters)
        self.desired_retention = desired_retention
        self.decay = -self.w[20]
        self.factor = 0.9 ** (1 / self.decay) - 1
        self.maximum_interval = 36500

    def review(self, card: Card, rating_name: str, today: int) -> Card:
        rating = RATING[rating_name]
        card = Card(
            state=card.state,
            stability=card.stability,
            difficulty=card.difficulty,
            last_day=card.last_day,
            due_day=card.due_day,
            interval=card.interval,
        )
        days_since = None if card.last_day is None else max(0, today - card.last_day)
        if card.state in ("New", "Learning", "Relearning"):
            if card.stability is None or card.difficulty is None:
                card.stability = self._initial_stability(rating)
                card.difficulty = self._initial_difficulty(rating, clamp=True)
            elif days_since is not None and days_since < 1:
                card.stability = self._short_term_stability(card.stability, rating)
                card.difficulty = self._next_difficulty(card.difficulty, rating)
            else:
                assert card.stability is not None and card.difficulty is not None
                retrievability = self._retrievability(card.stability, days_since or 0)
                card.stability = self._next_stability(card.difficulty, card.stability, retrievability, rating)
                card.difficulty = self._next_difficulty(card.difficulty, rating)
            # Empty learning / relearning steps: graduate immediately.
            card.state = "Review"
            card.interval = self._next_interval(card.stability)
        elif card.state == "Review":
            assert card.stability is not None and card.difficulty is not None
            if days_since is not None and days_since < 1:
                card.stability = self._short_term_stability(card.stability, rating)
            else:
                retrievability = self._retrievability(card.stability, days_since or 0)
                card.stability = self._next_stability(card.difficulty, card.stability, retrievability, rating)
            card.difficulty = self._next_difficulty(card.difficulty, rating)
            card.interval = self._next_interval(card.stability)
        else:
            raise ValueError(card.state)
        card.due_day = today + card.interval
        card.last_day = today
        return card

    def _clamp_stability(self, stability: float) -> float:
        return max(stability, STABILITY_MIN)

    def _clamp_difficulty(self, difficulty: float) -> float:
        return min(max(difficulty, 1.0), 10.0)

    def _initial_stability(self, rating: int) -> float:
        return self._clamp_stability(self.w[rating - 1])

    def _initial_difficulty(self, rating: int, clamp: bool) -> float:
        value = self.w[4] - math.exp(self.w[5] * (rating - 1)) + 1
        return self._clamp_difficulty(value) if clamp else value

    def _next_interval(self, stability: float) -> int:
        nxt = (stability / self.factor) * (self.desired_retention ** (1 / self.decay) - 1)
        nxt = round(nxt)
        return min(max(nxt, 1), self.maximum_interval)

    def _short_term_stability(self, stability: float, rating: int) -> float:
        increase = math.exp(self.w[17] * (rating - 3 + self.w[18])) * (stability ** -self.w[19])
        if rating in (2, 3, 4):
            increase = max(increase, 1.0)
        return self._clamp_stability(stability * increase)

    def _next_difficulty(self, difficulty: float, rating: int) -> float:
        arg_1 = self._initial_difficulty(4, clamp=False)
        delta = -(self.w[6] * (rating - 3))
        arg_2 = difficulty + (10.0 - difficulty) * delta / 9.0
        nxt = self.w[7] * arg_1 + (1 - self.w[7]) * arg_2
        return self._clamp_difficulty(nxt)

    def _retrievability(self, stability: float, elapsed_days: int) -> float:
        return (1 + self.factor * elapsed_days / stability) ** self.decay

    def _next_stability(self, difficulty: float, stability: float, retrievability: float, rating: int) -> float:
        if rating == 1:
            long_term = (
                self.w[11]
                * (difficulty ** -self.w[12])
                * (((stability + 1) ** self.w[13]) - 1)
                * math.exp((1 - retrievability) * self.w[14])
            )
            short_term = stability / math.exp(self.w[17] * self.w[18])
            nxt = min(long_term, short_term)
        else:
            hard_penalty = self.w[15] if rating == 2 else 1
            easy_bonus = self.w[16] if rating == 4 else 1
            nxt = stability * (
                1
                + math.exp(self.w[8])
                * (11 - difficulty)
                * (stability ** -self.w[9])
                * (math.exp((1 - retrievability) * self.w[10]) - 1)
                * hard_penalty
                * easy_bonus
            )
        return self._clamp_stability(nxt)
