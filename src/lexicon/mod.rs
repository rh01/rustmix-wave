//! Multilingual SD-card lexicon. This is separate from the X4 dictionary pack.

pub mod catalog;
pub mod format;
pub mod normalize;
pub mod store;
pub mod ui;
pub mod wordlist;

pub use catalog::{scan_catalog, DictMeta, LexiconCatalog};
pub use format::{crc32, parse_lexicon, Header, ParsedLexicon, LEXICON_PAGE_INDEX_MAX_BYTES};
pub use normalize::normalize_key;
pub use store::{
    load_index, lookup_exact, lookup_prefix, read_entry, LexiconIndex, LexiconStore,
    PAGE_INDEX_WORKER_THRESHOLD,
};
pub use ui::{LexiconUiState, STATIC_CREDITS};
pub use wordlist::{parse_wordlist, WordListFile};

/// Product tree for RMXLEX1 dictionaries and RMXWLS1 lists.
pub const LEXICON_ROOT: &str = "/sdcard/RUSTMIX/LEXICON";
