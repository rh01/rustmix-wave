//! Today's queue: due reviews first, then new cards from the word list.

use std::collections::BTreeSet;

use super::{
    fsrs::{Fsrs6, MemoryState, Phase, Rating},
    progress::StoredCard,
    scheduler::{review_sm2, Algo},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueKind {
    Review,
    New,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueItem {
    pub dict_slot: u8,
    pub entry_id: u32,
    pub kind: QueueKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StudySession {
    pub queue: Vec<QueueItem>,
    pub cursor: usize,
    pub requeued: BTreeSet<(u8, u32)>,
}

impl StudySession {
    #[must_use]
    pub fn current(&self) -> Option<&QueueItem> {
        self.queue.get(self.cursor)
    }

    #[must_use]
    pub fn finished(&self) -> bool {
        self.cursor >= self.queue.len()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeckStats {
    pub reviewed_today: u32,
    pub streak: u32,
    pub new_count: u32,
    pub learning: u32,
    pub review: u32,
    pub relearning: u32,
    pub due_next_7: [u32; 7],
}

/// Due reviews (oldest first, capped by `max_reviews`) precede new cards
/// (word-list order, capped by `new_per_day`). A card appears at most once.
pub fn build_session(
    list: &[(u8, u32)],
    cards: &[StoredCard],
    today: u32,
    new_per_day: u32,
    max_reviews: u32,
) -> StudySession {
    let list_set: BTreeSet<(u8, u32)> = list.iter().copied().collect();
    let mut due: Vec<&StoredCard> = cards
        .iter()
        .filter(|card| {
            list_set.contains(&card.key())
                && card.state != phase_byte(Phase::New)
                && card.due_day <= today
        })
        .collect();
    due.sort_by_key(|card| (card.due_day, card.entry_id, card.dict_slot));
    let mut queue = Vec::new();
    let mut seen = BTreeSet::new();
    for card in due.into_iter().take(max_reviews as usize) {
        if seen.insert(card.key()) {
            queue.push(QueueItem {
                dict_slot: card.dict_slot,
                entry_id: card.entry_id,
                kind: QueueKind::Review,
            });
        }
    }
    let mut added_new = 0u32;
    for &(dict_slot, entry_id) in list {
        if added_new >= new_per_day {
            break;
        }
        let key = (dict_slot, entry_id);
        if seen.contains(&key) {
            continue;
        }
        let existing = cards.iter().find(|card| card.key() == key);
        let is_new = existing.is_none_or(|card| card.state == phase_byte(Phase::New));
        if !is_new {
            continue;
        }
        seen.insert(key);
        queue.push(QueueItem {
            dict_slot,
            entry_id,
            kind: QueueKind::New,
        });
        added_new += 1;
    }
    StudySession {
        queue,
        cursor: 0,
        requeued: BTreeSet::new(),
    }
}

/// Apply one rating. Again may append the card once at the end of the queue.
pub fn apply_rating(
    session: &mut StudySession,
    cards: &mut Vec<StoredCard>,
    rating: Rating,
    today: u32,
    algo: Algo,
    retention: f64,
) -> bool {
    let Some(item) = session.current().cloned() else {
        return true;
    };
    let position = cards
        .iter()
        .position(|card| card.dict_slot == item.dict_slot && card.entry_id == item.entry_id);
    let mut stored = position
        .map(|index| cards[index].clone())
        .unwrap_or(StoredCard {
            dict_slot: item.dict_slot,
            state: phase_byte(Phase::New),
            reps: 0,
            entry_id: item.entry_id,
            stability_bits: 0,
            difficulty_bits: 0,
            due_day: 0,
            last_day: 0,
            lapses: 0,
        });
    let elapsed = if stored.last_day == 0 || stored.state == phase_byte(Phase::New) {
        0
    } else {
        today.saturating_sub(stored.last_day)
    };
    let memory = review_memory(&stored, rating, today, algo, retention);
    stored.state = phase_byte(memory.phase);
    stored.reps = memory.reps;
    stored.stability_bits = f32_bits(memory.stability.unwrap_or(0.0));
    stored.difficulty_bits = f32_bits(memory.difficulty.unwrap_or(0.0));
    stored.due_day = today.saturating_add(memory.interval.max(0) as u32);
    stored.last_day = today;
    if rating == Rating::Again {
        stored.lapses = stored.lapses.saturating_add(1);
    }
    if let Some(index) = position {
        cards[index] = stored;
    } else {
        cards.push(stored);
    }
    if rating == Rating::Again && session.requeued.insert((item.dict_slot, item.entry_id)) {
        session.queue.push(QueueItem {
            kind: QueueKind::Review,
            ..item
        });
    }
    session.cursor += 1;
    let _ = elapsed;
    session.finished()
}

pub fn review_memory(
    card: &StoredCard,
    rating: Rating,
    today: u32,
    algo: Algo,
    retention: f64,
) -> MemoryState {
    let memory = MemoryState {
        phase: phase_from_byte(card.state),
        stability: nonzero_f32(card.stability()),
        difficulty: nonzero_f32(card.difficulty()),
        last_day: if card.last_day == 0 {
            None
        } else {
            Some(card.last_day)
        },
        interval: card.difficulty().round().max(0.0) as i32,
        reps: card.reps,
    };
    match algo {
        Algo::Fsrs6 => {
            let mut next = Fsrs6::new(retention).review(&memory, rating, today);
            next.reps = card.reps.saturating_add(1);
            next
        }
        Algo::Sm2 => review_sm2(&memory, rating, today),
    }
}

pub fn stats(cards: &[StoredCard], review_days: &[u32], today: u32) -> DeckStats {
    let mut counts = DeckStats {
        reviewed_today: review_days.iter().filter(|day| **day == today).count() as u32,
        streak: streak(review_days, today),
        new_count: 0,
        learning: 0,
        review: 0,
        relearning: 0,
        due_next_7: [0; 7],
    };
    for card in cards {
        match phase_from_byte(card.state) {
            Phase::New => counts.new_count += 1,
            Phase::Learning => counts.learning += 1,
            Phase::Review => counts.review += 1,
            Phase::Relearning => counts.relearning += 1,
        }
        if card.due_day >= today {
            let ahead = card.due_day - today;
            if ahead < 7 {
                counts.due_next_7[ahead as usize] += 1;
            }
        }
    }
    counts
}

fn streak(days: &[u32], today: u32) -> u32 {
    let mut unique: Vec<u32> = days.to_vec();
    unique.sort_unstable();
    unique.dedup();
    if unique.is_empty() {
        return 0;
    }
    let mut cursor = if unique.last() == Some(&today) {
        today
    } else if unique.last() == Some(&(today.saturating_sub(1))) {
        today.saturating_sub(1)
    } else {
        return 0;
    };
    let mut count = 0u32;
    while unique.binary_search(&cursor).is_ok() {
        count += 1;
        if cursor == 0 {
            break;
        }
        cursor -= 1;
    }
    count
}

fn f32_bits(value: f64) -> u32 {
    (value as f32).to_bits()
}

fn nonzero_f32(value: f32) -> Option<f64> {
    if value == 0.0 {
        None
    } else {
        Some(f64::from(value))
    }
}

pub fn phase_byte(phase: Phase) -> u8 {
    match phase {
        Phase::New => 0,
        Phase::Learning => 1,
        Phase::Review => 2,
        Phase::Relearning => 3,
    }
}

pub fn phase_from_byte(value: u8) -> Phase {
    match value {
        1 => Phase::Learning,
        2 => Phase::Review,
        3 => Phase::Relearning,
        _ => Phase::New,
    }
}

/// Days since 1970-01-01 for a civil date. Invalid dates return `None`.
#[must_use]
pub fn unix_day(year: i32, month: u32, day: u32) -> Option<u32> {
    if !(1..=12).contains(&month) || day == 0 || day > 31 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    if days < 0 {
        None
    } else {
        Some(days as u32)
    }
}

fn days_from_civil(year: i32, month: u32, day: u32) -> i32 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = (year - era * 400) as u32;
    let month_prime = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * month_prime + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe as i32 - 719468
}

#[cfg(test)]
mod tests {
    use super::{apply_rating, build_session, unix_day, QueueKind, StoredCard};
    use crate::vocab::{fsrs::Rating, scheduler::Algo};

    fn card(slot: u8, id: u32, state: u8, due: u32) -> StoredCard {
        StoredCard {
            dict_slot: slot,
            state,
            reps: 2,
            entry_id: id,
            stability_bits: 3.0f32.to_bits(),
            difficulty_bits: 5.0f32.to_bits(),
            due_day: due,
            last_day: due.saturating_sub(1),
            lapses: 0,
        }
    }

    #[test]
    fn reviews_precede_new_cards_and_caps_apply() {
        let list = [(0, 3), (0, 4), (0, 5), (0, 1), (0, 2)];
        let cards = vec![card(0, 1, 2, 100), card(0, 2, 2, 90), card(0, 9, 2, 80)];
        let session = build_session(&list, &cards, 100, 2, 1);
        assert_eq!(session.queue.len(), 3);
        assert_eq!(session.queue[0].entry_id, 2);
        assert_eq!(session.queue[0].kind, QueueKind::Review);
        assert_eq!(session.queue[1].entry_id, 3);
        assert_eq!(session.queue[2].entry_id, 4);
        assert!(session.queue.iter().all(|item| item.entry_id != 1));
        assert!(session.queue.iter().all(|item| item.entry_id != 5));
    }

    #[test]
    fn queue_does_not_repeat_a_card() {
        let list = [(0, 1), (0, 1)];
        let session = build_session(&list, &[], 10, 20, 20);
        assert_eq!(session.queue.len(), 1);
    }

    #[test]
    fn again_requeues_once_at_the_end() {
        let list = [(0, 1), (0, 2)];
        let mut session = build_session(&list, &[], 10, 10, 10);
        let mut cards = Vec::new();
        assert!(!apply_rating(
            &mut session,
            &mut cards,
            Rating::Again,
            10,
            Algo::Fsrs6,
            0.9
        ));
        assert_eq!(session.queue.len(), 3);
        assert_eq!(session.queue[2].entry_id, 1);
        assert!(!apply_rating(
            &mut session,
            &mut cards,
            Rating::Good,
            10,
            Algo::Fsrs6,
            0.9
        ));
        let before = session.queue.len();
        assert!(apply_rating(
            &mut session,
            &mut cards,
            Rating::Again,
            10,
            Algo::Fsrs6,
            0.9
        ));
        assert_eq!(session.queue.len(), before);
        assert_eq!(session.cursor, session.queue.len());
    }

    #[test]
    fn unix_epoch_day_is_zero() {
        assert_eq!(unix_day(1970, 1, 1), Some(0));
        assert_eq!(unix_day(1970, 1, 2), Some(1));
        assert_eq!(unix_day(2024, 1, 1), Some(19723));
    }
}
