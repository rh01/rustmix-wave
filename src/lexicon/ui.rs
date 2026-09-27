//! Rotary keyboard state for the multilingual lexicon screen.

use std::{fs::File, path::Path};

use anyhow::Result;

use crate::{
    buttons::ButtonEvent,
    keyboard_navigation::KeyboardGridNavigation,
    lexicon::{
        catalog::{scan_catalog, DictMeta},
        format::{
            EntryRecord, FIELD_DEF_EN, FIELD_DEF_ZH, FIELD_EXAMPLE, FIELD_HEADWORD, FIELD_PHONETIC,
            FIELD_POS, FIELD_READING,
        },
        lookup_exact, lookup_prefix, normalize_key, read_entry,
        store::poll_index,
        LexiconIndex, LEXICON_ROOT,
    },
};

const LATIN_KEYS: &[&str] = &[
    "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S",
    "T", "U", "V", "W", "X", "Y", "Z", "DEL", "CLR", "GO", "*", "MODE", "DICT", "SRC",
];
const LATIN_COLUMNS: usize = 8;
const KANA_KEYS: &[&str] = &[
    "あ", "い", "う", "え", "お", "か", "き", "く", "け", "こ", "さ", "し", "す", "せ", "そ", "た",
    "ち", "つ", "て", "と", "な", "に", "ぬ", "ね", "の", "は", "ひ", "ふ", "へ", "ほ", "ま", "み",
    "む", "め", "も", "や", "ゆ", "よ", "゛", "小", "ら", "り", "る", "れ", "ろ", "わ", "を", "ん",
    "DEL", "CLR", "GO", "*", "MODE", "DICT", "SRC",
];
const KANA_COLUMNS: usize = 5;
const QUERY_MAX_CHARS: usize = 48;
pub const RESULT_LIMIT: usize = 8;

/// Shown on the sources screen even when the SD card has no dictionaries.
pub const STATIC_CREDITS: &[&str] = &[
    "ECDICT: MIT, skywind3000",
    "JMdict: CC BY-SA 4.0, Electronic Dictionary Research and Development Group",
    "JLPT vocabulary lists: Jonathan Waller, tanos.co.uk (Creative Commons BY, https://www.tanos.co.uk/jlpt/sharing/); CSV packaging: jamsinclair/open-anki-jlpt-decks (MIT)",
    "KANJIDIC2: CC BY-SA 4.0, Electronic Dictionary Research and Development Group",
    "CC-CEDICT: CC BY-SA 4.0, MDBG",
    "English speech: LJ Speech public domain, Piper en_US-ljspeech-medium",
    "Japanese speech: MeloTTS JP, MIT, MyShell.ai",
    "Chinese speech: MeloTTS ZH, MIT, MyShell.ai",
];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum KeyboardMode {
    #[default]
    Latin,
    Kana,
    Pinyin,
}

impl KeyboardMode {
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Latin => Self::Kana,
            Self::Kana => Self::Pinyin,
            Self::Pinyin => Self::Latin,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Latin => "ABC",
            Self::Kana => "かな",
            Self::Pinyin => "PY",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum InputFocus {
    #[default]
    Keyboard,
    Results,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LexiconCommand {
    None,
    OpenEntry,
    OpenSources,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LexiconHit {
    pub entry_id: u32,
    pub label: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ShownEntry {
    pub dict_id: String,
    pub entry_id: u32,
    pub headword: String,
    pub reading: String,
    pub phonetic: String,
    pub pos: String,
    pub defs: Vec<String>,
    pub examples: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LexiconUiState {
    pub query: String,
    pub mode: KeyboardMode,
    pub focus: InputFocus,
    pub navigation: KeyboardGridNavigation,
    pub dicts: Vec<DictMeta>,
    pub dict_index: usize,
    pub hits: Vec<LexiconHit>,
    pub hit_index: usize,
    pub entry: Option<ShownEntry>,
    pub entry_page: usize,
    pub source_index: usize,
    pub message: String,
    pub index: Option<LexiconIndex>,
    /// True when the open entry has a clip. False shows no audio mark.
    pub pronounce_ready: bool,
    /// Lookup waiting for a large page index to load: `Some(prefix_only)`.
    pub pending_lookup: Option<bool>,
}

impl Default for LexiconUiState {
    fn default() -> Self {
        Self {
            query: String::new(),
            mode: KeyboardMode::Latin,
            focus: InputFocus::Keyboard,
            navigation: KeyboardGridNavigation::new(LATIN_KEYS.len(), LATIN_COLUMNS),
            dicts: Vec::new(),
            dict_index: 0,
            hits: Vec::new(),
            hit_index: 0,
            entry: None,
            entry_page: 0,
            source_index: 0,
            message: "No lexicon on SD".into(),
            index: None,
            pronounce_ready: false,
            pending_lookup: None,
        }
    }
}

impl LexiconUiState {
    pub fn refresh_catalog(&mut self) {
        self.refresh_catalog_from(Path::new(LEXICON_ROOT));
    }

    pub fn refresh_catalog_from(&mut self, root: &Path) {
        match scan_catalog(root) {
            Ok(catalog) => {
                self.dicts = catalog.dicts;
                if self.dict_index >= self.dicts.len() {
                    self.dict_index = 0;
                }
                self.message = if self.dicts.is_empty() {
                    "No lexicon on SD".into()
                } else {
                    format!("{} dictionaries", self.dicts.len())
                };
                self.index = None;
            }
            Err(error) => {
                self.dicts.clear();
                self.index = None;
                self.message = compact(&error.to_string());
            }
        }
    }

    #[must_use]
    pub fn current_dict(&self) -> Option<&DictMeta> {
        self.dicts.get(self.dict_index)
    }

    #[must_use]
    pub fn keys(&self) -> &'static [&'static str] {
        match self.mode {
            KeyboardMode::Kana => KANA_KEYS,
            KeyboardMode::Latin | KeyboardMode::Pinyin => LATIN_KEYS,
        }
    }

    #[must_use]
    pub fn columns(&self) -> usize {
        match self.mode {
            KeyboardMode::Kana => KANA_COLUMNS,
            KeyboardMode::Latin | KeyboardMode::Pinyin => LATIN_COLUMNS,
        }
    }

    #[must_use]
    pub fn selected_label(&self) -> &'static str {
        self.keys()[self.navigation.selected()]
    }

    #[must_use]
    pub const fn navigation_mode_label(&self) -> &'static str {
        self.navigation.status_label()
    }

    pub fn toggle_navigation_axis(&mut self) {
        self.navigation.toggle_axis();
    }

    pub fn apply_button(&mut self, event: ButtonEvent) -> LexiconCommand {
        if self.focus == InputFocus::Results && !self.hits.is_empty() {
            return self.apply_results(event);
        }
        match event {
            ButtonEvent::Up => self.navigation.move_previous(),
            ButtonEvent::Down => self.navigation.move_next(),
            ButtonEvent::Select => return self.activate_selected_key(),
        }
        LexiconCommand::None
    }

    #[must_use]
    pub fn entry_page_count(&self) -> usize {
        let Some(entry) = &self.entry else {
            return 1;
        };
        let lines = 3 + entry.defs.len() + entry.examples.len();
        lines.div_ceil(6).max(1)
    }

    pub fn change_entry_page(&mut self, delta: isize, page_count: usize) {
        let pages = page_count.max(1);
        let next = self.entry_page as isize + delta;
        self.entry_page = next.clamp(0, pages as isize - 1) as usize;
    }

    fn apply_results(&mut self, event: ButtonEvent) -> LexiconCommand {
        match event {
            ButtonEvent::Up => {
                if self.hit_index == 0 {
                    self.focus = InputFocus::Keyboard;
                } else {
                    self.hit_index -= 1;
                }
            }
            ButtonEvent::Down => {
                if self.hit_index + 1 < self.hits.len() {
                    self.hit_index += 1;
                }
            }
            ButtonEvent::Select => {
                if self.load_selected_entry().is_ok() {
                    return LexiconCommand::OpenEntry;
                }
            }
        }
        LexiconCommand::None
    }

    fn activate_selected_key(&mut self) -> LexiconCommand {
        match self.selected_label() {
            "DEL" => {
                self.query.pop();
                self.clear_hits("Deleted last character");
            }
            "CLR" => {
                self.query.clear();
                self.clear_hits("Cleared search");
            }
            "GO" => self.run_lookup(false),
            "*" => self.run_lookup(true),
            "MODE" => {
                self.mode = self.mode.next();
                self.navigation = KeyboardGridNavigation::new(self.keys().len(), self.columns());
                self.message = format!("Keyboard {}", self.mode.label());
            }
            "DICT" => self.cycle_dict(),
            "SRC" => return LexiconCommand::OpenSources,
            "゛" => apply_dakuten(&mut self.query),
            "小" => apply_small(&mut self.query),
            label => self.push_label(label),
        }
        LexiconCommand::None
    }

    fn push_label(&mut self, label: &str) {
        if self.query.chars().count() >= QUERY_MAX_CHARS {
            return;
        }
        let text = if self.mode == KeyboardMode::Pinyin {
            label.to_ascii_lowercase()
        } else {
            label.to_string()
        };
        self.query.push_str(&text);
        self.clear_hits("GO lookup, * prefix");
    }

    fn cycle_dict(&mut self) {
        if self.dicts.is_empty() {
            self.message = "No lexicon on SD".into();
            return;
        }
        self.dict_index = (self.dict_index + 1) % self.dicts.len();
        self.index = None;
        self.pending_lookup = None;
        self.clear_hits(&format!("Dictionary {}", self.dicts[self.dict_index].id));
    }

    fn clear_hits(&mut self, message: &str) {
        self.hits.clear();
        self.hit_index = 0;
        self.focus = InputFocus::Keyboard;
        self.message = message.into();
    }

    fn run_lookup(&mut self, prefix_only: bool) {
        if let Err(error) = self.lookup_inner(prefix_only) {
            self.message = compact(&error.to_string());
            self.hits.clear();
        }
    }

    fn lookup_inner(&mut self, prefix_only: bool) -> Result<()> {
        let Some(dict) = self.dicts.get(self.dict_index) else {
            self.message = "No lexicon on SD".into();
            return Ok(());
        };
        let path = dict.lex_path.clone();
        let dict_id = dict.id.clone();
        if !self.ensure_index(Path::new(&path))? {
            self.pending_lookup = Some(prefix_only);
            self.hits.clear();
            self.focus = InputFocus::Keyboard;
            self.message = "Opening dictionary...".into();
            return Ok(());
        }
        let index = self.index.as_ref().expect("index loaded");
        let mut file = File::open(&path)?;
        let key = normalize_key(&self.query);
        let mut ids = if prefix_only {
            Vec::new()
        } else {
            lookup_exact(&mut file, index, key.as_bytes())?
        };
        let used_prefix = ids.is_empty();
        if used_prefix {
            ids = lookup_prefix(&mut file, index, key.as_bytes(), RESULT_LIMIT)?
                .into_iter()
                .map(|(_key, id)| id)
                .collect();
        }
        ids.truncate(RESULT_LIMIT);
        let mut hits = Vec::new();
        for id in ids {
            let entry = read_entry(&mut file, &index.header, id)?;
            hits.push(LexiconHit {
                entry_id: id,
                label: field_text(&entry, FIELD_HEADWORD).unwrap_or_else(|| key.clone()),
            });
        }
        self.hits = hits;
        self.hit_index = 0;
        if self.hits.is_empty() {
            self.focus = InputFocus::Keyboard;
            self.message = format!("{dict_id}: not found");
        } else {
            self.focus = InputFocus::Results;
            self.message = if used_prefix || prefix_only {
                format!("{} prefix hits", self.hits.len())
            } else {
                format!("{} exact hits", self.hits.len())
            };
        }
        Ok(())
    }

    /// `Ok(false)` while a large index is still loading on its worker.
    fn ensure_index(&mut self, path: &Path) -> Result<bool> {
        let display = path.display().to_string();
        if self
            .index
            .as_ref()
            .is_some_and(|index| index.path == display)
        {
            return Ok(true);
        }
        match poll_index(path)? {
            Some(index) => {
                self.index = Some(index);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Resume a lookup that was waiting for its page index. Returns true when
    /// the screen changed.
    pub fn tick(&mut self) -> bool {
        let Some(prefix_only) = self.pending_lookup else {
            return false;
        };
        let Some(path) = self
            .dicts
            .get(self.dict_index)
            .map(|dict| dict.lex_path.clone())
        else {
            self.pending_lookup = None;
            return false;
        };
        match self.ensure_index(Path::new(&path)) {
            Ok(false) => false,
            Ok(true) => {
                self.pending_lookup = None;
                self.run_lookup(prefix_only);
                true
            }
            Err(error) => {
                self.pending_lookup = None;
                self.message = compact(&error.to_string());
                true
            }
        }
    }

    fn load_selected_entry(&mut self) -> Result<()> {
        let Some(dict) = self.dicts.get(self.dict_index) else {
            return Ok(());
        };
        let hit = self
            .hits
            .get(self.hit_index)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no hit"))?;
        let path = dict.lex_path.clone();
        let dict_id = dict.id.clone();
        if !self.ensure_index(Path::new(&path))? {
            anyhow::bail!("dictionary is still opening");
        }
        let index = self.index.as_ref().expect("index");
        let mut file = File::open(&path)?;
        let entry = read_entry(&mut file, &index.header, hit.entry_id)?;
        self.entry = Some(shown_from_entry(&dict_id, hit.entry_id, &entry));
        self.entry_page = 0;
        Ok(())
    }
}

fn shown_from_entry(dict_id: &str, entry_id: u32, entry: &EntryRecord) -> ShownEntry {
    let mut shown = ShownEntry {
        dict_id: dict_id.into(),
        entry_id,
        headword: field_text(entry, FIELD_HEADWORD).unwrap_or_default(),
        ..ShownEntry::default()
    };
    let mut readings = Vec::new();
    for field in &entry.fields {
        match field.id {
            FIELD_READING => readings.push(field.text()),
            FIELD_PHONETIC => shown.phonetic = field.text(),
            FIELD_POS if shown.pos.is_empty() => shown.pos = field.text(),
            FIELD_DEF_ZH | FIELD_DEF_EN => shown.defs.push(field.text()),
            FIELD_EXAMPLE => shown.examples.push(field.text()),
            _ => {}
        }
    }
    shown.reading = readings.join("  ");
    shown
}

fn field_text(entry: &EntryRecord, id: u8) -> Option<String> {
    entry
        .fields
        .iter()
        .find(|field| field.id == id)
        .map(super::format::Field::text)
}

fn compact(value: &str) -> String {
    value.chars().take(80).collect()
}

fn apply_dakuten(query: &mut String) {
    let Some(last) = query.chars().last() else {
        return;
    };
    let next = match last {
        'か' => 'が',
        'が' => 'か',
        'き' => 'ぎ',
        'ぎ' => 'き',
        'く' => 'ぐ',
        'ぐ' => 'く',
        'け' => 'げ',
        'げ' => 'け',
        'こ' => 'ご',
        'ご' => 'こ',
        'さ' => 'ざ',
        'ざ' => 'さ',
        'し' => 'じ',
        'じ' => 'し',
        'す' => 'ず',
        'ず' => 'す',
        'せ' => 'ぜ',
        'ぜ' => 'せ',
        'そ' => 'ぞ',
        'ぞ' => 'そ',
        'た' => 'だ',
        'だ' => 'た',
        'ち' => 'ぢ',
        'ぢ' => 'ち',
        'つ' => 'づ',
        'づ' => 'つ',
        'て' => 'で',
        'で' => 'て',
        'と' => 'ど',
        'ど' => 'と',
        'は' => 'ば',
        'ば' => 'ぱ',
        'ぱ' => 'は',
        'ひ' => 'び',
        'び' => 'ぴ',
        'ぴ' => 'ひ',
        'ふ' => 'ぶ',
        'ぶ' => 'ぷ',
        'ぷ' => 'ふ',
        'へ' => 'べ',
        'べ' => 'ぺ',
        'ぺ' => 'へ',
        'ほ' => 'ぼ',
        'ぼ' => 'ぽ',
        'ぽ' => 'ほ',
        _ => last,
    };
    if next != last {
        query.pop();
        query.push(next);
    }
}

fn apply_small(query: &mut String) {
    let Some(last) = query.chars().last() else {
        return;
    };
    let next = match last {
        'あ' => 'ぁ',
        'ぁ' => 'あ',
        'い' => 'ぃ',
        'ぃ' => 'い',
        'う' => 'ぅ',
        'ぅ' => 'う',
        'え' => 'ぇ',
        'ぇ' => 'え',
        'お' => 'ぉ',
        'ぉ' => 'お',
        'や' => 'ゃ',
        'ゃ' => 'や',
        'ゆ' => 'ゅ',
        'ゅ' => 'ゆ',
        'よ' => 'ょ',
        'ょ' => 'よ',
        'つ' => 'っ',
        'っ' => 'つ',
        'わ' => 'ゎ',
        'ゎ' => 'わ',
        _ => last,
    };
    if next != last {
        query.pop();
        query.push(next);
    }
}

#[cfg(test)]
mod tests {
    use super::{KeyboardMode, LexiconUiState, LATIN_KEYS};
    use crate::buttons::ButtonEvent;
    use crate::lexicon::catalog::DictMeta;

    fn press_label(state: &mut LexiconUiState, label: &str) {
        let index = state
            .keys()
            .iter()
            .position(|key| *key == label)
            .unwrap_or_else(|| panic!("missing {label}"));
        state.navigation.jump_to(index);
        state.apply_button(ButtonEvent::Select);
    }

    #[test]
    fn keyboard_modes_cycle() {
        let mut state = LexiconUiState::default();
        assert_eq!(state.mode, KeyboardMode::Latin);
        press_label(&mut state, "MODE");
        assert_eq!(state.mode, KeyboardMode::Kana);
        press_label(&mut state, "MODE");
        assert_eq!(state.mode, KeyboardMode::Pinyin);
        press_label(&mut state, "MODE");
        assert_eq!(state.mode, KeyboardMode::Latin);
        assert_eq!(LATIN_KEYS.len(), 33);
    }

    #[test]
    fn kana_mode_appends_hiragana() {
        let mut state = LexiconUiState::default();
        press_label(&mut state, "MODE");
        assert_eq!(state.selected_label(), "あ");
        state.apply_button(ButtonEvent::Select);
        assert_eq!(state.query, "あ");
        press_label(&mut state, "か");
        assert_eq!(state.query, "あか");
        press_label(&mut state, "゛");
        assert_eq!(state.query, "あが");
        press_label(&mut state, "や");
        press_label(&mut state, "小");
        assert_eq!(state.query, "あがゃ");
    }

    #[test]
    fn dict_key_cycles_dictionaries() {
        let mut state = LexiconUiState::default();
        state.dicts = vec![
            DictMeta {
                id: "ECDICT".into(),
                title: "ECDICT".into(),
                ..DictMeta::default()
            },
            DictMeta {
                id: "JMDICT".into(),
                title: "JMdict".into(),
                ..DictMeta::default()
            },
        ];
        press_label(&mut state, "DICT");
        assert_eq!(state.current_dict().unwrap().id, "JMDICT");
        press_label(&mut state, "DICT");
        assert_eq!(state.current_dict().unwrap().id, "ECDICT");
    }

    #[test]
    fn boot_axis_toggle_preserves_selected_key() {
        let mut state = LexiconUiState::default();
        state.apply_button(ButtonEvent::Down);
        let selected = state.navigation.selected();
        state.toggle_navigation_axis();
        assert_eq!(state.navigation.selected(), selected);
        assert_eq!(state.navigation_mode_label(), "NAV V");
    }
}
