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
/// Preferred per-chapter JSON object size. Larger objects are still scanned
/// for the scalar fields (`chapterUid`, title, index); nested anchors are not kept.
pub const MAX_CHAPTER_OBJECT_BYTES: usize = 16 * 1024;
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
/// Per-read timeout after the TLS handshake. Cancel is a flag on the main
/// task; the worker notices it once `esp_http_client_read` returns, which is
/// at most this long. The handshake still uses [`HTTP_TIMEOUT_SECS`].
pub const HTTP_READ_TIMEOUT_MS: i32 = 3_000;
/// Consecutive body-read time with no bytes. Each 3 second read timeout counts,
/// and any read that returns bytes resets it. Past this, the chapter fails
/// into the normal retry/skip path instead of looping.
pub const HTTP_IDLE_LIMIT_MS: u64 = 20_000;
/// Whole WeRead job, shared by every body read of one chapter download.
/// A server that trickles a few bytes keeps resetting the idle window, so this
/// cap is what lets `needs_radio` drop.
pub const HTTP_CHAPTER_LIMIT_MS: u64 = 120_000;
/// First try plus two retries. A failed chapter does not cancel the book.
pub const DOWNLOAD_ATTEMPTS: u8 = 3;
pub const DOWNLOAD_RETRY_MS: u64 = 1_000;
/// Offline download writes this many bytes to the SD card at a time.
/// The worker does not retain the chapter shard.
pub const DOWNLOAD_CHUNK_BYTES: usize = 8 * 1024;
/// Leading bytes kept on the worker to tell a zip, txt envelope, and shard apart.
pub const DOWNLOAD_CLASSIFY_BYTES: usize = 512;
/// One long-lived `weread-http` pthread stack, allocated from PSRAM.
pub const WEREAD_HTTP_WORKER_STACK_BYTES: usize = 32 * 1024;
/// `esp_http_client` RX and TX buffers. Each stays under
/// `CONFIG_SPIRAM_MALLOC_ALWAYSINTERNAL` so TLS I/O uses internal RAM.
pub const HTTP_IO_BUFFER_BYTES: usize = 2 * 1024;
/// Waveshare ESP32-S3-WROOM-1-N16R8 octal PSRAM.
pub const MODULE_PSRAM_BYTES: usize = 8 * 1024 * 1024;

pub const WEREAD_ROOT: &str = "/sdcard/RUSTMIX/WEREAD";
pub const WEREAD_CONFIG_PATH: &str = "/sdcard/RUSTMIX/WEREAD.TXT";
pub const SESSION_FILE: &str = "SESS.TXT";
pub const SESSION_TMP: &str = "SESS.TMP";
pub const SESSION_BAK: &str = "SESS.BAK";

#[cfg(test)]
mod tests {
    use super::{
        DOWNLOAD_CHUNK_BYTES, HTTP_CHAPTER_LIMIT_MS, HTTP_IDLE_LIMIT_MS, HTTP_IO_BUFFER_BYTES,
        HTTP_READ_TIMEOUT_MS, HTTP_TIMEOUT_SECS, MAX_CHAPTER_TEXT, MAX_SHARD_BYTES,
        MODULE_PSRAM_BYTES, WEREAD_HTTP_WORKER_STACK_BYTES,
    };
    use crate::fonts::{
        GLYPH_CACHE_BUDGET_BYTES, MAX_SD_FONT_BYTES, SD_FONT_RESIDENT_BUDGET_BYTES,
    };

    #[test]
    fn sd_font_worker_and_tls_fit_beside_a_chapter() {
        assert_eq!(MAX_SD_FONT_BYTES, 2 * 1024 * 1024);
        assert!(SD_FONT_RESIDENT_BUDGET_BYTES > MAX_SD_FONT_BYTES);
        // Download holds a handful of 8 KiB chunks, not the shard or decoded text.
        let download = SD_FONT_RESIDENT_BUDGET_BYTES
            + WEREAD_HTTP_WORKER_STACK_BYTES
            + DOWNLOAD_CHUNK_BYTES * 4
            + GLYPH_CACHE_BUDGET_BYTES;
        assert!(
            download < MODULE_PSRAM_BYTES,
            "download psram budget {download} exceeds {MODULE_PSRAM_BYTES}"
        );
        // Opening one stored chapter decodes a single shard after download has finished.
        let opened = SD_FONT_RESIDENT_BUDGET_BYTES
            + MAX_SHARD_BYTES
            + MAX_CHAPTER_TEXT
            + GLYPH_CACHE_BUDGET_BYTES;
        assert!(
            opened < MODULE_PSRAM_BYTES,
            "open-chapter psram budget {opened} exceeds {MODULE_PSRAM_BYTES}"
        );
        // TLS I/O and download chunks stay under the internal-malloc threshold.
        assert!(HTTP_IO_BUFFER_BYTES * 2 <= 16 * 1024);
        assert!((4 * 1024..=8 * 1024).contains(&DOWNLOAD_CHUNK_BYTES));
        assert!(DOWNLOAD_CHUNK_BYTES <= 16 * 1024);
        assert!(WEREAD_HTTP_WORKER_STACK_BYTES <= 32 * 1024);
        assert!((2_000..=3_000).contains(&HTTP_READ_TIMEOUT_MS));
        assert!(HTTP_READ_TIMEOUT_MS < (HTTP_TIMEOUT_SECS as i32) * 1_000);
        assert_eq!(HTTP_IDLE_LIMIT_MS, 20_000);
        assert_eq!(HTTP_CHAPTER_LIMIT_MS, 120_000);
        assert!(HTTP_CHAPTER_LIMIT_MS > HTTP_IDLE_LIMIT_MS);
        assert!(HTTP_IDLE_LIMIT_MS > HTTP_READ_TIMEOUT_MS as u64);
    }
}
