#!/usr/bin/env python3
"""Write tests/fixtures/vocab/fsrs6_vectors.json.

Prefers the installed `fsrs` package (py-fsrs, MIT). If it is missing, falls
back to the line-by-line port in rmxlex.fsrs and records that in the file.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

SEQUENCES = [
    ["Again"],
    ["Hard"],
    ["Good"],
    ["Easy"],
    ["Good", "Good", "Good"],
    ["Again", "Good", "Good"],
    ["Good", "Again", "Good"],
    ["Easy", "Easy", "Hard"],
    ["Hard", "Good", "Easy", "Good"],
    ["Good", "Hard", "Hard", "Good"],
    ["Again", "Again", "Good"],
    ["Easy", "Good", "Again", "Easy"],
    ["Good", "Good", "Easy", "Good", "Good"],
    ["Hard", "Hard", "Good", "Good"],
    ["Good", "Easy", "Good", "Hard", "Again"],
    ["Again", "Hard", "Good", "Easy"],
    ["Easy", "Again", "Again", "Good"],
    ["Good", "Good", "Good", "Again", "Good", "Good"],
    ["Hard", "Easy", "Good"],
    ["Good", "Good"],
    ["Easy", "Hard", "Good", "Easy"],
    ["Again", "Good", "Hard", "Easy", "Good"],
]


def _from_py_fsrs() -> tuple[str, list[dict]]:
    from datetime import datetime, timedelta, timezone

    from fsrs import Card, Rating, Scheduler

    rating = {
        "Again": Rating.Again,
        "Hard": Rating.Hard,
        "Good": Rating.Good,
        "Easy": Rating.Easy,
    }
    scheduler = Scheduler(learning_steps=(), relearning_steps=(), enable_fuzzing=False)
    origin = datetime(2024, 1, 1, tzinfo=timezone.utc)
    sequences = []
    for names in SEQUENCES:
        card = Card()
        when = origin
        steps = []
        for name in names:
            before = when
            card, _log = scheduler.review_card(card, rating[name], review_datetime=when)
            interval = (card.due - before).days
            steps.append(
                {
                    "rating": name,
                    "elapsed_days": 0 if not steps else (before - origin).days - steps[-1]["day"],
                    "stability": float(card.stability),
                    "difficulty": float(card.difficulty),
                    "interval": int(interval),
                    "day": (before - origin).days,
                }
            )
            when = card.due
        # elapsed_days for step 0 is 0; later steps are the gap from previous review day.
        sequences.append({"ratings": names, "steps": steps})
    return "py-fsrs", sequences


def _from_local() -> tuple[str, list[dict]]:
    from rmxlex.fsrs import Card, Scheduler

    scheduler = Scheduler()
    sequences = []
    for names in SEQUENCES:
        card = Card()
        today = 0
        steps = []
        previous_day = 0
        for index, name in enumerate(names):
            elapsed = 0 if index == 0 else today - previous_day
            card = scheduler.review(card, name, today)
            steps.append(
                {
                    "rating": name,
                    "elapsed_days": elapsed,
                    "stability": card.stability,
                    "difficulty": card.difficulty,
                    "interval": card.interval,
                    "day": today,
                }
            )
            previous_day = today
            today = card.due_day
        sequences.append({"ratings": names, "steps": steps})
    return "rmxlex.fsrs", sequences


def main() -> int:
    try:
        source, sequences = _from_py_fsrs()
    except Exception as error:  # noqa: BLE001 - optional dependency
        print(f"py-fsrs unavailable ({error}); using local port", file=sys.stderr)
        source, sequences = _from_local()
    payload = {
        "source": source,
        "algorithm": "FSRS-6",
        "desired_retention": 0.9,
        "learning_steps": [],
        "relearning_steps": [],
        "enable_fuzzing": False,
        "sequences": sequences,
    }
    root = Path(__file__).resolve().parents[2]
    dest = root / "tests" / "fixtures" / "vocab" / "fsrs6_vectors.json"
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    print(f"wrote {dest} source={source} sequences={len(sequences)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
