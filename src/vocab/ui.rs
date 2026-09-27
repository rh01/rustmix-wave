//! Vocabulary trainer UI state: deck pick, card face, and rating cursor.

use std::{fs::File, path::Path};

use crate::{
    buttons::ButtonEvent,
    lexicon::wordlist::parse_wordlist,
    vocab::{
        fsrs::Rating,
        progress::{
            add_myword, append_review_log, dict_slots, ensure_dict_slot, load_progress,
            load_settings, read_mywords, recent_review_days, save_progress, ProgressFile,
            StoredCard, VocabSettings, VOCAB_ROOT,
        },
        session::{self, stats, unix_day, DeckStats, QueueItem, StudySession},
    },
};

pub const RATINGS: [&str; 4] = ["Again", "Hard", "Good", "Easy"];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CardFace {
    #[default]
    Front,
    Back,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeckChoice {
    pub name: String,
    pub title: String,
    pub dict_id: String,
    pub path: String,
    pub mywords: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VocabUiState {
    pub decks: Vec<DeckChoice>,
    pub deck_index: usize,
    pub cards: Vec<StoredCard>,
    pub settings: VocabSettings,
    pub session: Option<StudySession>,
    pub face: CardFace,
    pub rating_cursor: usize,
    pub today: Option<u32>,
    pub message: String,
    pub reviews_since_save: u32,
    pub completed_today: u32,
    pub root: String,
    pub show_stats: bool,
    /// True when the current card has a clip. False shows no audio mark.
    pub pronounce_ready: bool,
}

impl Default for VocabUiState {
    fn default() -> Self {
        Self {
            decks: Vec::new(),
            deck_index: 0,
            cards: Vec::new(),
            settings: VocabSettings::default(),
            session: None,
            face: CardFace::Front,
            rating_cursor: 2,
            today: None,
            message: "时钟未设置".into(),
            reviews_since_save: 0,
            completed_today: 0,
            root: VOCAB_ROOT.into(),
            show_stats: false,
            pronounce_ready: false,
        }
    }
}

impl VocabUiState {
    pub fn load_default(&mut self) {
        self.load_from(Path::new(VOCAB_ROOT));
    }

    pub fn load_from(&mut self, root: &Path) {
        self.root = root.display().to_string();
        self.settings = load_settings(root);
        self.cards = load_progress(root)
            .map(|file| file.cards)
            .unwrap_or_default();
        self.decks = discover_decks(root);
        if self.deck_index >= self.decks.len() {
            self.deck_index = 0;
        }
        if self.today.is_none() {
            self.message = "时钟未设置".into();
        }
    }

    pub fn sync_clock(&mut self, year: i32, month: u32, day: u32, integrity_lost: bool) {
        if integrity_lost {
            self.today = None;
            self.message = "时钟未设置".into();
            return;
        }
        match unix_day(year, month, day) {
            Some(today) => {
                self.today = Some(today);
                if self.message == "时钟未设置" {
                    self.message = "Ready".into();
                }
            }
            None => {
                self.today = None;
                self.message = "时钟未设置".into();
            }
        }
    }

    pub fn apply_deck_button(&mut self, event: ButtonEvent) -> bool {
        let count = self.decks.len().saturating_add(1);
        match event {
            ButtonEvent::Up => {
                self.deck_index = self.deck_index.checked_sub(1).unwrap_or(count - 1);
            }
            ButtonEvent::Down => self.deck_index = (self.deck_index + 1) % count.max(1),
            ButtonEvent::Select => {
                if self.decks.is_empty() || self.deck_index >= self.decks.len() {
                    self.show_stats = true;
                    return false;
                }
                return self.start_selected();
            }
        }
        false
    }

    pub fn start_selected(&mut self) -> bool {
        let Some(today) = self.today else {
            self.message = "时钟未设置".into();
            return false;
        };
        let Some(deck) = self.decks.get(self.deck_index).cloned() else {
            self.message = "No word list".into();
            return false;
        };
        let root = Path::new(&self.root).to_path_buf();
        let list = if deck.mywords {
            myword_pairs(&root).unwrap_or_default()
        } else {
            wordlist_pairs(&root, &deck).unwrap_or_default()
        };
        self.session = Some(session::build_session(
            &list,
            &self.cards,
            today,
            self.settings.new_per_day,
            self.settings.max_reviews,
        ));
        self.face = CardFace::Front;
        self.rating_cursor = 2;
        self.message = format!(
            "{} cards",
            self.session
                .as_ref()
                .map(|item| item.queue.len())
                .unwrap_or(0)
        );
        true
    }

    /// Test helper that starts a session from an in-memory list.
    pub fn start_session(&mut self, list: Vec<(u8, u32)>, today: u32) {
        self.today = Some(today);
        self.session = Some(session::build_session(
            &list,
            &self.cards,
            today,
            self.settings.new_per_day,
            self.settings.max_reviews,
        ));
        self.face = CardFace::Front;
        self.rating_cursor = 2;
    }

    pub fn apply_session_button(&mut self, event: ButtonEvent) -> bool {
        let Some(today) = self.today else {
            self.message = "时钟未设置".into();
            return false;
        };
        match event {
            ButtonEvent::Up if self.face == CardFace::Back => {
                self.rating_cursor = self.rating_cursor.checked_sub(1).unwrap_or(3);
            }
            ButtonEvent::Down if self.face == CardFace::Back => {
                self.rating_cursor = (self.rating_cursor + 1) % 4;
            }
            ButtonEvent::Select if self.face == CardFace::Front => {
                self.face = CardFace::Back;
                self.rating_cursor = 2;
            }
            ButtonEvent::Select => {
                let rating = match self.rating_cursor {
                    0 => Rating::Again,
                    1 => Rating::Hard,
                    3 => Rating::Easy,
                    _ => Rating::Good,
                };
                let Some(mut session) = self.session.take() else {
                    return false;
                };
                let current = session.current().cloned();
                let finished = session::apply_rating(
                    &mut session,
                    &mut self.cards,
                    rating,
                    today,
                    self.settings.algo,
                    self.settings.retention(),
                );
                self.session = Some(session);
                if let Some(item) = current {
                    let elapsed = 0;
                    let _ = append_review_log(
                        Path::new(&self.root),
                        today,
                        item.dict_slot,
                        item.entry_id,
                        rating_name(rating),
                        elapsed,
                    );
                }
                self.completed_today = self.completed_today.saturating_add(1);
                self.reviews_since_save = self.reviews_since_save.saturating_add(1);
                self.face = CardFace::Front;
                self.rating_cursor = 2;
                if finished {
                    self.message = "Session complete".into();
                    self.save();
                    return true;
                }
                if self.reviews_since_save >= 10 {
                    self.save();
                }
            }
            _ => {}
        }
        false
    }

    pub fn save(&mut self) {
        let progress = ProgressFile {
            algo: self.settings.algo,
            cards: self.cards.clone(),
        };
        if save_progress(Path::new(&self.root), &progress).is_ok() {
            self.reviews_since_save = 0;
        }
    }

    pub fn save_myword(&self, dict_id: &str, entry_id: u32) -> ResultSave {
        match add_myword(Path::new(&self.root), dict_id, entry_id) {
            Ok(()) => ResultSave::Saved,
            Err(error) => ResultSave::Failed(error.to_string()),
        }
    }

    #[must_use]
    pub fn current_item(&self) -> Option<&QueueItem> {
        self.session.as_ref().and_then(StudySession::current)
    }

    /// Dictionary id and entry id for the card on screen.
    #[must_use]
    pub fn pronounce_target(&self) -> Option<(String, u32)> {
        let item = self.current_item()?;
        let deck = self.decks.get(self.deck_index)?;
        let dict_id = if deck.mywords {
            dict_slots(Path::new(&self.root))
                .ok()?
                .get(usize::from(item.dict_slot))?
                .clone()
        } else if deck.dict_id.is_empty() {
            return None;
        } else {
            deck.dict_id.clone()
        };
        Some((dict_id, item.entry_id))
    }

    #[must_use]
    pub fn stats(&self) -> DeckStats {
        let today = self.today.unwrap_or(0);
        let days = recent_review_days(Path::new(&self.root));
        let mut summary = stats(&self.cards, &days, today);
        if summary.reviewed_today == 0 {
            summary.reviewed_today = self.completed_today;
        }
        summary
    }

    #[must_use]
    pub fn due_and_new_counts(&self) -> (u32, u32) {
        let Some(today) = self.today else {
            return (0, 0);
        };
        let due = self
            .cards
            .iter()
            .filter(|card| card.state != 0 && card.due_day <= today)
            .count() as u32;
        (due, self.settings.new_per_day)
    }
}

pub enum ResultSave {
    Saved,
    Failed(String),
}

fn rating_name(rating: Rating) -> &'static str {
    match rating {
        Rating::Again => "Again",
        Rating::Hard => "Hard",
        Rating::Good => "Good",
        Rating::Easy => "Easy",
    }
}

const MAX_WORDLIST_BYTES: usize = 1024 * 1024;

fn read_capped(path: &Path, limit: usize) -> std::io::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let mut limited = std::io::Read::take(file, limit as u64 + 1);
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut limited, &mut bytes)?;
    if bytes.len() > limit {
        bytes.truncate(limit);
    }
    Ok(bytes)
}

fn discover_decks(root: &Path) -> Vec<DeckChoice> {
    let mut decks = Vec::new();
    let lists = root.join("../LEXICON/LISTS");
    if lists.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&lists) {
            let mut paths: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
            paths.sort();
            for path in paths {
                if path.extension().and_then(|ext| ext.to_str()) != Some("WLS") {
                    continue;
                }
                if let Ok(bytes) = read_capped(&path, MAX_WORDLIST_BYTES) {
                    if let Ok(parsed) = parse_wordlist(&bytes) {
                        decks.push(DeckChoice {
                            name: path
                                .file_stem()
                                .and_then(|v| v.to_str())
                                .unwrap_or("LIST")
                                .into(),
                            title: parsed.title,
                            dict_id: parsed.dict_id,
                            path: path.display().to_string(),
                            mywords: false,
                        });
                    }
                }
                if decks.len() >= 64 {
                    break;
                }
            }
        }
    }
    decks.push(DeckChoice {
        name: "MYWORDS".into(),
        title: "My words".into(),
        dict_id: String::new(),
        path: String::new(),
        mywords: true,
    });
    decks
}

fn wordlist_pairs(root: &Path, deck: &DeckChoice) -> anyhow::Result<Vec<(u8, u32)>> {
    let bytes = read_capped(Path::new(&deck.path), MAX_WORDLIST_BYTES)
        .map_err(|error| anyhow::anyhow!(error))?;
    let parsed = parse_wordlist(&bytes)?;
    let slot = ensure_dict_slot(root, &parsed.dict_id)?;
    Ok(parsed
        .entry_ids
        .into_iter()
        .map(|entry_id| (slot, entry_id))
        .collect())
}

fn myword_pairs(root: &Path) -> anyhow::Result<Vec<(u8, u32)>> {
    let slots = dict_slots(root)?;
    let mut pairs = Vec::new();
    for (dict_id, entry_id) in read_mywords(root)? {
        let slot = slots
            .iter()
            .position(|item| item == &dict_id)
            .map(|index| index as u8)
            .unwrap_or(0);
        pairs.push((slot, entry_id));
    }
    Ok(pairs)
}

/// Open a headword for the active card. Missing files yield an empty string.
#[must_use]
pub fn headword_for(lex_path: &Path, entry_id: u32) -> String {
    let Ok(mut file) = File::open(lex_path) else {
        return String::new();
    };
    let Ok(index) = crate::lexicon::load_index(lex_path) else {
        return String::new();
    };
    let Ok(entry) = crate::lexicon::store::read_entry(&mut file, &index.header, entry_id) else {
        return String::new();
    };
    entry
        .fields
        .iter()
        .find(|field| field.id == crate::lexicon::format::FIELD_HEADWORD)
        .map(|field| field.text())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::VocabUiState;
    use crate::buttons::ButtonEvent;
    use crate::vocab::fsrs::Rating;

    #[test]
    fn session_starts_on_good_and_advances() {
        let mut ui = VocabUiState::default();
        ui.root = std::env::temp_dir()
            .join(format!("rmx-ui-{}", std::process::id()))
            .display()
            .to_string();
        let _ = std::fs::create_dir_all(&ui.root);
        ui.start_session(vec![(0, 1), (0, 2)], 20_000);
        assert!(ui.current_item().is_some());
        assert!(!ui.apply_session_button(ButtonEvent::Select));
        assert_eq!(ui.rating_cursor, 2);
        ui.apply_session_button(ButtonEvent::Select);
        assert_eq!(ui.current_item().map(|item| item.entry_id), Some(2));
        let _ = Rating::Good;
        let _ = std::fs::remove_dir_all(&ui.root);
    }

    #[test]
    fn missing_clock_refuses_to_schedule() {
        let mut ui = VocabUiState::default();
        ui.decks.push(super::DeckChoice {
            name: "CET4".into(),
            title: "CET4".into(),
            dict_id: "ECDICT".into(),
            path: "missing.wls".into(),
            mywords: false,
        });
        ui.today = None;
        assert!(!ui.start_selected());
        assert_eq!(ui.message, "时钟未设置");
    }

    #[test]
    fn rating_cursor_moves_on_the_back() {
        let mut ui = VocabUiState::default();
        ui.today = Some(1);
        ui.session = Some(crate::vocab::session::StudySession {
            queue: vec![crate::vocab::session::QueueItem {
                dict_slot: 0,
                entry_id: 1,
                kind: crate::vocab::session::QueueKind::New,
            }],
            cursor: 0,
            requeued: Default::default(),
        });
        ui.face = super::CardFace::Back;
        ui.rating_cursor = 2;
        ui.apply_session_button(ButtonEvent::Down);
        assert_eq!(ui.rating_cursor, 3);
        ui.apply_session_button(ButtonEvent::Up);
        assert_eq!(ui.rating_cursor, 2);
    }

    #[test]
    fn wordlist_reads_are_capped_and_oversized_lists_are_skipped() {
        let base = std::env::temp_dir().join(format!("rmx-wls-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let lists = base.join("LEXICON/LISTS");
        std::fs::create_dir_all(&lists).unwrap();
        std::fs::create_dir_all(base.join("VOCAB")).unwrap();
        std::fs::write(
            lists.join("MINI.WLS"),
            include_bytes!("../../tests/fixtures/lexicon/MINI.WLS"),
        )
        .unwrap();
        let huge_path = lists.join("HUGE.WLS");
        std::fs::write(&huge_path, vec![0u8; super::MAX_WORDLIST_BYTES + 64]).unwrap();
        let capped = super::read_capped(&huge_path, super::MAX_WORDLIST_BYTES).unwrap();
        assert_eq!(capped.len(), super::MAX_WORDLIST_BYTES);
        let mut ui = VocabUiState::default();
        ui.load_from(&base.join("VOCAB"));
        let names: Vec<_> = ui.decks.iter().map(|deck| deck.name.as_str()).collect();
        assert!(names.contains(&"MINI"));
        assert!(!names.contains(&"HUGE"));
        let _ = std::fs::remove_dir_all(&base);
    }
}
