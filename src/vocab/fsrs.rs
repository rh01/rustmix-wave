//! FSRS-6 day scheduler.
//!
//! Ported line by line from py-fsrs (MIT) `Scheduler` with empty
//! `learning_steps`, empty `relearning_steps`, and fuzzing disabled. The
//! device does not schedule minute-level learning steps: every interval is a
//! whole day in `[1, 36500]`. Same-day reviews (`elapsed < 1`) use short-term
//! stability, matching py-fsrs. Source:
//! <https://github.com/open-spaced-repetition/py-fsrs>.

const DEFAULT_PARAMETERS: [f64; 21] = [
    0.212, 1.2931, 2.3065, 8.2956, 6.4133, 0.8334, 3.0194, 0.001, 1.8722, 0.1666, 0.796, 1.4835,
    0.0614, 0.2629, 1.6483, 0.6014, 1.8729, 0.5425, 0.0912, 0.0658, 0.1542,
];
const STABILITY_MIN: f64 = 0.001;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rating {
    Again = 1,
    Hard = 2,
    Good = 3,
    Easy = 4,
}

impl Rating {
    fn value(self) -> i32 {
        self as i32
    }

    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "Again" => Some(Self::Again),
            "Hard" => Some(Self::Hard),
            "Good" => Some(Self::Good),
            "Easy" => Some(Self::Easy),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    New,
    Learning,
    Review,
    Relearning,
}

#[derive(Clone, Debug)]
pub struct MemoryState {
    pub phase: Phase,
    pub stability: Option<f64>,
    pub difficulty: Option<f64>,
    pub last_day: Option<u32>,
    pub interval: i32,
    pub reps: u16,
}

impl Default for MemoryState {
    fn default() -> Self {
        Self {
            phase: Phase::New,
            stability: None,
            difficulty: None,
            last_day: None,
            interval: 0,
            reps: 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Fsrs6 {
    w: [f64; 21],
    desired_retention: f64,
    decay: f64,
    factor: f64,
    maximum_interval: i32,
}

impl Default for Fsrs6 {
    fn default() -> Self {
        Self::new(0.9)
    }
}

impl Fsrs6 {
    #[must_use]
    pub fn new(desired_retention: f64) -> Self {
        let decay = -DEFAULT_PARAMETERS[20];
        Self {
            w: DEFAULT_PARAMETERS,
            desired_retention,
            decay,
            factor: 0.9_f64.powf(1.0 / decay) - 1.0,
            maximum_interval: 36500,
        }
    }

    #[must_use]
    pub fn review(&self, card: &MemoryState, rating: Rating, today: u32) -> MemoryState {
        let mut card = card.clone();
        let days_since = card.last_day.map(|day| today.saturating_sub(day));
        match card.phase {
            Phase::New | Phase::Learning | Phase::Relearning => {
                if card.stability.is_none() || card.difficulty.is_none() {
                    card.stability = Some(self.initial_stability(rating));
                    card.difficulty = Some(self.initial_difficulty(rating, true));
                } else if days_since.is_some_and(|days| days < 1) {
                    card.stability = Some(
                        self.short_term_stability(card.stability.unwrap_or(STABILITY_MIN), rating),
                    );
                    card.difficulty =
                        Some(self.next_difficulty(card.difficulty.unwrap_or(1.0), rating));
                } else {
                    let stability = card.stability.unwrap_or(STABILITY_MIN);
                    let difficulty = card.difficulty.unwrap_or(1.0);
                    let retrievability = self.retrievability(stability, days_since.unwrap_or(0));
                    card.stability =
                        Some(self.next_stability(difficulty, stability, retrievability, rating));
                    card.difficulty = Some(self.next_difficulty(difficulty, rating));
                }
                card.phase = Phase::Review;
                card.interval = self.next_interval(card.stability.unwrap_or(STABILITY_MIN));
            }
            Phase::Review => {
                let stability = card.stability.unwrap_or(STABILITY_MIN);
                let difficulty = card.difficulty.unwrap_or(1.0);
                if days_since.is_some_and(|days| days < 1) {
                    card.stability = Some(self.short_term_stability(stability, rating));
                } else {
                    let retrievability = self.retrievability(stability, days_since.unwrap_or(0));
                    card.stability =
                        Some(self.next_stability(difficulty, stability, retrievability, rating));
                }
                card.difficulty = Some(self.next_difficulty(difficulty, rating));
                card.interval = self.next_interval(card.stability.unwrap_or(STABILITY_MIN));
            }
        }
        card.last_day = Some(today);
        card
    }

    fn initial_stability(&self, rating: Rating) -> f64 {
        self.clamp_stability(self.w[rating.value() as usize - 1])
    }

    fn initial_difficulty(&self, rating: Rating, clamp: bool) -> f64 {
        let value = self.w[4] - (self.w[5] * f64::from(rating.value() - 1)).exp() + 1.0;
        if clamp {
            self.clamp_difficulty(value)
        } else {
            value
        }
    }

    fn next_interval(&self, stability: f64) -> i32 {
        let next =
            (stability / self.factor) * (self.desired_retention.powf(1.0 / self.decay) - 1.0);
        python_round(next).clamp(1, self.maximum_interval)
    }

    fn short_term_stability(&self, stability: f64, rating: Rating) -> f64 {
        let mut increase = (self.w[17] * (f64::from(rating.value() - 3) + self.w[18])).exp()
            * stability.powf(-self.w[19]);
        if matches!(rating, Rating::Hard | Rating::Good | Rating::Easy) {
            increase = increase.max(1.0);
        }
        self.clamp_stability(stability * increase)
    }

    fn next_difficulty(&self, difficulty: f64, rating: Rating) -> f64 {
        let arg_1 = self.initial_difficulty(Rating::Easy, false);
        let delta = -(self.w[6] * f64::from(rating.value() - 3));
        let arg_2 = difficulty + (10.0 - difficulty) * delta / 9.0;
        let next = self.w[7] * arg_1 + (1.0 - self.w[7]) * arg_2;
        self.clamp_difficulty(next)
    }

    fn retrievability(&self, stability: f64, elapsed_days: u32) -> f64 {
        (1.0 + self.factor * f64::from(elapsed_days) / stability).powf(self.decay)
    }

    fn next_stability(
        &self,
        difficulty: f64,
        stability: f64,
        retrievability: f64,
        rating: Rating,
    ) -> f64 {
        let next = if rating == Rating::Again {
            let long_term = self.w[11]
                * difficulty.powf(-self.w[12])
                * ((stability + 1.0).powf(self.w[13]) - 1.0)
                * ((1.0 - retrievability) * self.w[14]).exp();
            let short_term = stability / (self.w[17] * self.w[18]).exp();
            long_term.min(short_term)
        } else {
            let hard_penalty = if rating == Rating::Hard {
                self.w[15]
            } else {
                1.0
            };
            let easy_bonus = if rating == Rating::Easy {
                self.w[16]
            } else {
                1.0
            };
            stability
                * (1.0
                    + self.w[8].exp()
                        * (11.0 - difficulty)
                        * stability.powf(-self.w[9])
                        * (((1.0 - retrievability) * self.w[10]).exp() - 1.0)
                        * hard_penalty
                        * easy_bonus)
        };
        self.clamp_stability(next)
    }

    fn clamp_stability(&self, stability: f64) -> f64 {
        stability.max(STABILITY_MIN)
    }

    fn clamp_difficulty(&self, difficulty: f64) -> f64 {
        difficulty.clamp(1.0, 10.0)
    }
}

/// Python 3 `round` (half to even) so intervals match py-fsrs.
fn python_round(value: f64) -> i32 {
    if !value.is_finite() {
        return 1;
    }
    let floor = value.floor();
    let fraction = value - floor;
    let rounded = if (fraction - 0.5).abs() <= 1e-9 {
        if (floor as i64) % 2 == 0 {
            floor
        } else {
            floor + 1.0
        }
    } else {
        value.round()
    };
    rounded as i32
}

#[cfg(test)]
mod tests {
    use super::{python_round, Fsrs6, MemoryState, Rating};

    #[test]
    fn python_round_half_to_even() {
        assert_eq!(python_round(0.5), 0);
        assert_eq!(python_round(1.5), 2);
        assert_eq!(python_round(2.5), 2);
        assert_eq!(python_round(2.3065), 2);
    }

    #[test]
    fn first_reviews_match_default_parameters() {
        let fsrs = Fsrs6::default();
        let again = fsrs.review(&MemoryState::default(), Rating::Again, 0);
        assert!((again.stability.unwrap() - 0.212).abs() < 1e-9);
        assert!((again.difficulty.unwrap() - 6.4133).abs() < 1e-9);
        assert_eq!(again.interval, 1);
        let good = fsrs.review(&MemoryState::default(), Rating::Good, 0);
        assert!((good.stability.unwrap() - 2.3065).abs() < 1e-9);
        assert_eq!(good.interval, 2);
    }

    #[test]
    fn vectors_match_py_fsrs() {
        let text = include_str!("../../tests/fixtures/vocab/fsrs6_vectors.json");
        let sequences = parse_sequences(text);
        assert!(sequences.len() >= 20, "sequences {}", sequences.len());
        let fsrs = Fsrs6::default();
        for sequence in sequences {
            let mut card = MemoryState::default();
            for step in sequence {
                card = fsrs.review(&card, step.rating, step.day);
                let stability = card.stability.unwrap();
                let difficulty = card.difficulty.unwrap();
                let stability_error =
                    (stability - step.stability).abs() / step.stability.abs().max(1e-12);
                let difficulty_error =
                    (difficulty - step.difficulty).abs() / step.difficulty.abs().max(1e-12);
                assert!(
                    stability_error < 1e-3,
                    "S {stability} vs {} err {stability_error}",
                    step.stability
                );
                assert!(
                    difficulty_error < 1e-3,
                    "D {difficulty} vs {} err {difficulty_error}",
                    step.difficulty
                );
                assert_eq!(card.interval, step.interval);
            }
        }
    }

    struct Step {
        rating: Rating,
        stability: f64,
        difficulty: f64,
        interval: i32,
        day: u32,
    }

    fn parse_sequences(text: &str) -> Vec<Vec<Step>> {
        let mut sequences = Vec::new();
        let mut current = Vec::new();
        let mut rating = None;
        let mut stability = None;
        let mut difficulty = None;
        let mut interval = None;
        let mut day = None;
        for raw in text.lines() {
            let line = raw.trim().trim_end_matches(',');
            if let Some(value) = string_field(line, "rating") {
                rating = Rating::from_name(value);
            } else if let Some(value) = number_field(line, "stability") {
                stability = Some(value);
            } else if let Some(value) = number_field(line, "difficulty") {
                difficulty = Some(value);
            } else if let Some(value) = number_field(line, "interval") {
                interval = Some(value as i32);
            } else if let Some(value) = number_field(line, "day") {
                day = Some(value as u32);
            }
            if rating.is_some()
                && stability.is_some()
                && difficulty.is_some()
                && interval.is_some()
                && day.is_some()
            {
                current.push(Step {
                    rating: rating.take().unwrap(),
                    stability: stability.take().unwrap(),
                    difficulty: difficulty.take().unwrap(),
                    interval: interval.take().unwrap(),
                    day: day.take().unwrap(),
                });
            }
            if line == "]" && !current.is_empty() {
                sequences.push(std::mem::take(&mut current));
            }
        }
        sequences
    }

    fn string_field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
        let prefix = format!("\"{name}\":");
        let rest = line.trim().strip_prefix(&prefix)?.trim();
        rest.trim_matches('"').into()
    }

    fn number_field(line: &str, name: &str) -> Option<f64> {
        let prefix = format!("\"{name}\":");
        let rest = line.trim().strip_prefix(&prefix)?.trim();
        rest.parse().ok()
    }
}
