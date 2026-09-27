//! Scheduler switch used by the vocabulary trainer.

use super::{
    fsrs::{Fsrs6, MemoryState, Phase, Rating},
    sm2::Sm2State,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Algo {
    Fsrs6,
    Sm2,
}

impl Algo {
    #[must_use]
    pub fn parse(value: &str) -> Self {
        if value.eq_ignore_ascii_case("sm2") {
            Self::Sm2
        } else {
            Self::Fsrs6
        }
    }

    #[must_use]
    pub const fn file_code(self) -> u16 {
        match self {
            Self::Fsrs6 => 0,
            Self::Sm2 => 1,
        }
    }

    #[must_use]
    pub const fn from_file_code(code: u16) -> Self {
        match code {
            1 => Self::Sm2,
            _ => Self::Fsrs6,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Fsrs6 => "fsrs6",
            Self::Sm2 => "sm2",
        }
    }
}

pub trait Scheduler {
    fn review(&self, card: &MemoryState, rating: Rating, today: u32) -> MemoryState;
}

impl Scheduler for Fsrs6 {
    fn review(&self, card: &MemoryState, rating: Rating, today: u32) -> MemoryState {
        Fsrs6::review(self, card, rating, today)
    }
}

#[derive(Clone, Debug)]
pub struct Sm2Scheduler;

impl Scheduler for Sm2Scheduler {
    fn review(&self, card: &MemoryState, rating: Rating, today: u32) -> MemoryState {
        review_sm2(card, rating, today)
    }
}

#[must_use]
pub fn review_sm2(card: &MemoryState, rating: Rating, today: u32) -> MemoryState {
    let current = Sm2State {
        ef: card.stability.unwrap_or(super::sm2::EF_INITIAL),
        interval: card.interval,
        reps: card.reps,
    };
    let next = current.review(rating);
    MemoryState {
        phase: if rating == Rating::Again {
            Phase::Learning
        } else {
            Phase::Review
        },
        stability: Some(next.ef),
        difficulty: Some(f64::from(next.interval)),
        last_day: Some(today),
        interval: next.interval,
        reps: next.reps,
    }
}

#[cfg(test)]
mod tests {
    use super::{review_sm2, Algo, Scheduler};
    use crate::vocab::fsrs::{Fsrs6, MemoryState, Rating};

    #[test]
    fn algo_names_round_trip() {
        assert_eq!(Algo::parse("sm2"), Algo::Sm2);
        assert_eq!(Algo::parse("fsrs6"), Algo::Fsrs6);
        assert_eq!(Algo::from_file_code(Algo::Sm2.file_code()), Algo::Sm2);
    }

    #[test]
    fn trait_review_matches_fsrs() {
        let fsrs = Fsrs6::default();
        let direct = fsrs.review(&MemoryState::default(), Rating::Good, 10);
        let via_trait = Scheduler::review(&fsrs, &MemoryState::default(), Rating::Good, 10);
        assert_eq!(direct.interval, via_trait.interval);
    }

    #[test]
    fn sm2_again_resets_through_helper() {
        let mut card = MemoryState::default();
        card = review_sm2(&card, Rating::Good, 0);
        card = review_sm2(&card, Rating::Again, 1);
        assert_eq!(card.interval, 1);
        assert_eq!(card.phase, crate::vocab::fsrs::Phase::Learning);
    }
}
