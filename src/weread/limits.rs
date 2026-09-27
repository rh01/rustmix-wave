//! Hard bounds for untrusted WeRead responses and on-device buffers.

/// JSON metadata responses (shelf, catalog, notes, login).
pub const MAX_JSON_BYTES: usize = 256 * 1024;
/// Reader HTML scanned only for `psvts`.
pub const MAX_HTML_BYTES: usize = 384 * 1024;
/// One chapter shard (`e_*` / `t_*`) or EPUB zip payload.
pub const MAX_SHARD_BYTES: usize = 768 * 1024;
/// Plain text retained for one chapter.
pub const MAX_CHAPTER_TEXT: usize = 512 * 1024;
/// `META.TXT` book record. The portal can upload 64 MiB, so this read is capped.
pub const MAX_META_BYTES: usize = 8 * 1024;
/// Cover or inline image download.
pub const MAX_IMAGE_BYTES: usize = 96 * 1024;
/// Decoded image edge before downscale.
pub const MAX_IMAGE_EDGE: u32 = 800;
/// Shelf rows kept in RAM.
pub const MAX_SHELF_BOOKS: usize = 128;
/// Catalog entries kept for one book.
pub const MAX_CHAPTERS: usize = 1024;
/// Highlight and note rows.
pub const MAX_NOTES: usize = 64;
/// Inline images fetched for one chapter.
pub const MAX_CHAPTER_IMAGES: usize = 4;
/// Pages produced from one chapter.
pub const MAX_PAGES: usize = 2_048;
pub const MAX_TITLE_CHARS: usize = 80;
pub const MAX_FIELD_CHARS: usize = 160;
pub const MAX_NOTE_CHARS: usize = 240;
pub const MAX_ID_CHARS: usize = 96;
pub const MAX_COOKIE_CHARS: usize = 512;
pub const MAX_URL_CHARS: usize = 300;
pub const MAX_QR_CHARS: usize = 180;
/// `wr_skey` is documented to last about 1.5 h. Renew well before that.
pub const SKEY_RENEW_AFTER_SECS: u64 = 45 * 60;
pub const MIN_REQUEST_GAP_MS: u64 = 350;
pub const LOGIN_POLL_MS: u64 = 2_000;
pub const LOGIN_TIMEOUT_MS: u64 = 180_000;
pub const PROGRESS_DELAY_MS: u64 = 5_000;
pub const HTTP_TIMEOUT_SECS: u64 = 20;
/// First try plus two retries. A failed chapter does not cancel the book.
pub const DOWNLOAD_ATTEMPTS: u8 = 3;
pub const DOWNLOAD_RETRY_MS: u64 = 1_000;

pub const WEREAD_ROOT: &str = "/sdcard/RUSTMIX/WEREAD";
pub const WEREAD_CONFIG_PATH: &str = "/sdcard/RUSTMIX/WEREAD.TXT";
pub const SESSION_FILE: &str = "SESS.TXT";
pub const SESSION_TMP: &str = "SESS.TMP";
pub const SESSION_BAK: &str = "SESS.BAK";
