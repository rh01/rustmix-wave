//! SM-2 scheduler.
//!
//! EF' = EF + (0.1 − (5−q)·(0.08 + (5−q)·0.02)), with EF floored at 1.3.
//! Successful reviews with reps 0 / 1 / ≥2 schedule 1 day, 6 days, and
//! round(interval · EF). q < 3 resets reps to 0 and the interval to 1 day.
//! Ratings: Again = 1, Hard = 3, Good = 4, Easy = 5.

use super::fsrs::Rating;

pub const EF_FLOOR: f64 = 1.3;
pub const EF_INITIAL: f64 = 2.5;

#[derive(Clone, Debug)]
pub struct Sm2State {
    pub ef: f64,
    pub interval: i32,
    pub reps: u16,
}

impl Default for Sm2State {
    fn default() -> Self {
        Self {
            ef: EF_INITIAL,
            interval: 0,
            reps: 0,
        }
    }
}

impl Sm2State {
    #[must_use]
    pub fn review(&self, rating: Rating) -> Self {
        let quality = match rating {
            Rating::Again => 1,
            Rating::Hard => 3,
            Rating::Good => 4,
            Rating::Easy => 5,
        };
        let delta = 0.1 - f64::from(5 - quality) * (0.08 + f64::from(5 - quality) * 0.02);
        let ef = (self.ef + delta).max(EF_FLOOR);
        if quality < 3 {
            return Self {
                ef,
                interval: 1,
                reps: 0,
            };
        }
        let interval = match self.reps {
            0 => 1,
            1 => 6,
            _ => python_round(f64::from(self.interval.max(1)) * ef).max(1),
        };
        Self {
            ef,
            interval,
            reps: self.reps.saturating_add(1),
        }
    }
}

fn python_round(value: f64) -> i32 {
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
    use super::{Sm2State, EF_FLOOR, EF_INITIAL};
    use crate::vocab::fsrs::Rating;

    #[test]
    fn ef_floor_holds_after_again() {
        let mut card = Sm2State {
            ef: EF_FLOOR,
            interval: 10,
            reps: 4,
        };
        card = card.review(Rating::Again);
        assert!((card.ef - EF_FLOOR).abs() < 1e-9);
        assert_eq!(card.reps, 0);
        assert_eq!(card.interval, 1);
    }

    #[test]
    fn quality_below_three_resets() {
        let card = Sm2State {
            ef: 2.2,
            interval: 15,
            reps: 3,
        };
        let next = card.review(Rating::Again);
        assert_eq!(next.reps, 0);
        assert_eq!(next.interval, 1);
        assert!(next.ef < card.ef);
        assert!(next.ef >= EF_FLOOR);
    }

    #[test]
    fn first_success_is_one_day() {
        let next = Sm2State::default().review(Rating::Good);
        assert_eq!(next.interval, 1);
        assert_eq!(next.reps, 1);
        assert!((next.ef - EF_INITIAL).abs() < 1e-9);
    }

    #[test]
    fn second_success_is_six_days() {
        let next = Sm2State::default()
            .review(Rating::Good)
            .review(Rating::Good);
        assert_eq!(next.interval, 6);
        assert_eq!(next.reps, 2);
    }

    #[test]
    fn later_success_multiplies_interval_by_ef() {
        let next = Sm2State::default()
            .review(Rating::Good)
            .review(Rating::Good)
            .review(Rating::Good);
        assert_eq!(next.interval, 15);
        assert_eq!(next.reps, 3);
    }

    #[test]
    fn easy_raises_ef_before_the_interval_product() {
        let mut card = Sm2State::default();
        card = card.review(Rating::Easy);
        card = card.review(Rating::Easy);
        let ef_before = card.ef;
        card = card.review(Rating::Easy);
        assert!((card.ef - (ef_before + 0.1)).abs() < 1e-9);
        assert_eq!(card.interval, python_round_expected(6.0 * card.ef));
    }

    fn python_round_expected(value: f64) -> i32 {
        value.round() as i32
    }
}
