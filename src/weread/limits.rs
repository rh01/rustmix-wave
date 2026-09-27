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
/// Cover thumbnail download. Shelf covers stay small.
pub const MAX_COVER_BYTES: usize = 96 * 1024;
/// Historical name for the cover cap.
pub const MAX_IMAGE_BYTES: usize = MAX_COVER_BYTES;
/// One chapter image, compressed. Larger responses are skipped with a placeholder.
///
/// On device this buffer is a heap `Vec`. Allocations above
/// `CONFIG_SPIRAM_MALLOC_ALWAYSINTERNAL` (16 KiB) land in PSRAM. It is dropped
/// before the luma plane is allocated.
pub const MAX_CHAPTER_IMAGE_BYTES: usize = 1024 * 1024;
/// PNG source edge. A square RGBA buffer, the compressed file, and decoder
/// scratch still fit beside the SD font, glyph cache, and both WeRead stacks.
/// Larger headers are skipped.
pub const MAX_IMAGE_EDGE: u32 = 752;
/// Pixel count accepted for one PNG.
pub const MAX_DECODE_PIXELS: u32 = MAX_IMAGE_EDGE * MAX_IMAGE_EDGE;
/// One PNG output buffer (RGBA, 4 bytes per pixel). The compressed file is
/// still live during decode; the luma plane is not.
pub const MAX_IMAGE_DECODE_BYTES: usize = (MAX_DECODE_PIXELS as usize) * 4;
/// Baseline JPEG output edge.
///
/// `jpeg-decoder` keeps every component plane and the interleaved frame at the
/// same time, so a full-resolution 752px RGB image does not fit. Larger JPEGs
/// are decoded at 1/2, 1/4, or 1/8 when that output still fits, and skipped
/// when even 1/8 does not.
pub const MAX_JPEG_EDGE: u32 = 592;
/// Row filters, the zlib window, and one MCU-row of coefficients.
pub const IMAGE_DECODE_SCRATCH_BYTES: usize = 128 * 1024;
/// Widest reader measure. Bitmaps are decoded to this and scaled again to the
/// open layout when the chapter is drawn.
pub const IMAGE_TARGET_WIDTH: u32 = 752;
/// Tallest reader page the bitmap is scaled into before pagination.
pub const IMAGE_TARGET_HEIGHT: u32 = 594;
/// Shelf rows kept in RAM.
pub const MAX_SHELF_BOOKS: usize = 128;
/// Catalog entries kept for one book.
pub const MAX_CHAPTERS: usize = 1024;
/// Preferred per-chapter JSON object size. Larger objects are still scanned
/// for the scalar fields (`chapterUid`, title, index); nested anchors are not kept.
pub const MAX_CHAPTER_OBJECT_BYTES: usize = 16 * 1024;
/// Highlight and note rows.
pub const MAX_NOTES: usize = 64;
/// Inline images kept for one chapter. Each compressed body is fetched and
/// released before the next, so the cap bounds decoded bitmaps, not a pile of
/// 1 MiB files in RAM.
pub const MAX_CHAPTER_IMAGES: usize = 8;
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
/// `weread-img` pthread stack, allocated from PSRAM. JPEG/PNG decode runs here
/// so the 16 KiB main task stays free for buttons and page turns.
pub const IMAGE_DECODE_STACK_BYTES: usize = 64 * 1024;
/// `esp_http_client` RX and TX buffers. Each stays under
/// `CONFIG_SPIRAM_MALLOC_ALWAYSINTERNAL` so TLS I/O uses internal RAM.
pub const HTTP_IO_BUFFER_BYTES: usize = 2 * 1024;
/// Waveshare ESP32-S3-WROOM-1-N16R8 octal PSRAM.
pub const MODULE_PSRAM_BYTES: usize = 8 * 1024 * 1024;

/// PSRAM left for one chapter image after the resident font, glyph cache, and
/// both WeRead stacks. The compressed file and the decoder's planes share it.
#[must_use]
pub fn image_heap_room() -> usize {
    MODULE_PSRAM_BYTES.saturating_sub(
        crate::fonts::SD_FONT_RESIDENT_BUDGET_BYTES
            + WEREAD_HTTP_WORKER_STACK_BYTES
            + IMAGE_DECODE_STACK_BYTES
            + crate::fonts::GLYPH_CACHE_BUDGET_BYTES,
    )
}

/// Output edge `jpeg-decoder` produces for `idct` (8 = full, 4 = 1/2, 2 = 1/4, 1 = 1/8).
#[must_use]
pub fn jpeg_scaled_edge(len: u16, idct: u32) -> u32 {
    u32::from(len).saturating_mul(idct).saturating_sub(1) / 8 + 1
}

/// Bytes `jpeg-decoder` allocates besides the compressed file.
///
/// Baseline holds component planes plus the interleaved frame. Progressive
/// also holds full-resolution DCT coefficients, which scaling does not shrink.
/// The MCU-row term is the scan-time peak when it exceeds those two frames.
#[must_use]
pub fn jpeg_decoder_extra_bytes(
    src_w: u16,
    src_h: u16,
    out_w: u32,
    out_h: u32,
    components: usize,
    progressive: bool,
) -> Option<usize> {
    let out = (out_w as usize).checked_mul(out_h as usize)?;
    let planes = out.checked_mul(components)?;
    let interleaved = planes;
    let coeff = if progressive {
        let blocks_x = (usize::from(src_w)).div_ceil(8);
        let blocks_y = (usize::from(src_h)).div_ceil(8);
        blocks_x
            .checked_mul(blocks_y)?
            .checked_mul(64)?
            .checked_mul(2)?
            .checked_mul(components)?
    } else {
        0
    };
    // One MCU row, worst vertical sampling of 4, 64 i16 coefficients per block.
    let mcu_row = (usize::from(src_w))
        .div_ceil(8)
        .checked_mul(4)?
        .checked_mul(64)?
        .checked_mul(2)?
        .checked_mul(components)?;
    let combine = planes.checked_add(interleaved)?.checked_add(coeff)?;
    let scan = planes.checked_add(mcu_row)?;
    Some(combine.max(scan))
}

/// `file_len` plus `extra` plus decoder scratch fits in [`image_heap_room`].
#[must_use]
pub fn image_alloc_fits(file_len: usize, extra: usize) -> bool {
    file_len
        .saturating_add(extra)
        .saturating_add(IMAGE_DECODE_SCRATCH_BYTES)
        <= image_heap_room()
}

pub const WEREAD_ROOT: &str = "/sdcard/RUSTMIX/WEREAD";
pub const WEREAD_CONFIG_PATH: &str = "/sdcard/RUSTMIX/WEREAD.TXT";
pub const SESSION_FILE: &str = "SESS.TXT";
pub const SESSION_TMP: &str = "SESS.TMP";
pub const SESSION_BAK: &str = "SESS.BAK";

#[cfg(test)]
mod tests {
    use super::{
        image_alloc_fits, image_heap_room, jpeg_decoder_extra_bytes, jpeg_scaled_edge,
        DOWNLOAD_CHUNK_BYTES, HTTP_CHAPTER_LIMIT_MS, HTTP_IDLE_LIMIT_MS, HTTP_IO_BUFFER_BYTES,
        HTTP_READ_TIMEOUT_MS, HTTP_TIMEOUT_SECS, IMAGE_DECODE_SCRATCH_BYTES,
        IMAGE_DECODE_STACK_BYTES, IMAGE_TARGET_HEIGHT, IMAGE_TARGET_WIDTH, MAX_CHAPTER_IMAGES,
        MAX_CHAPTER_IMAGE_BYTES, MAX_CHAPTER_TEXT, MAX_DECODE_PIXELS, MAX_IMAGE_DECODE_BYTES,
        MAX_JPEG_EDGE, MAX_SHARD_BYTES, MODULE_PSRAM_BYTES, WEREAD_HTTP_WORKER_STACK_BYTES,
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
        assert_eq!(
            MAX_IMAGE_DECODE_BYTES,
            MAX_DECODE_PIXELS as usize * 4,
            "png cap is one RGBA buffer"
        );
        // PNG: file + RGBA output + scratch. No second full frame.
        let png_extra = MAX_DECODE_PIXELS as usize * 4;
        assert!(
            image_alloc_fits(MAX_CHAPTER_IMAGE_BYTES, png_extra),
            "png square at the edge must fit"
        );
        // The file is dropped before the luma plane, so RGBA and luma coexist
        // without the compressed bytes.
        let png_and_luma = png_extra + MAX_DECODE_PIXELS as usize;
        assert!(png_and_luma + IMAGE_DECODE_SCRATCH_BYTES <= image_heap_room());
        assert!(
            !image_alloc_fits(MAX_CHAPTER_IMAGE_BYTES, 800 * 800 * 4),
            "800x800 rgba plus the file does not fit"
        );
        // Baseline JPEG: component planes and the interleaved frame coexist
        // with the file. 800x800 full-resolution RGB does not fit; 1/2 does.
        let full_800 = jpeg_decoder_extra_bytes(800, 800, 800, 800, 3, false).unwrap();
        assert!(
            !image_alloc_fits(MAX_CHAPTER_IMAGE_BYTES, full_800),
            "800x800 baseline jpeg planes plus interleaved frame do not fit"
        );
        let half = jpeg_scaled_edge(800, 4);
        let half_800 = jpeg_decoder_extra_bytes(800, 800, half, half, 3, false).unwrap();
        assert!(image_alloc_fits(MAX_CHAPTER_IMAGE_BYTES, half_800));
        assert!(half <= MAX_JPEG_EDGE);
        let jpeg_edge = jpeg_decoder_extra_bytes(
            MAX_JPEG_EDGE as u16,
            MAX_JPEG_EDGE as u16,
            MAX_JPEG_EDGE,
            MAX_JPEG_EDGE,
            3,
            false,
        )
        .unwrap();
        assert!(image_alloc_fits(MAX_CHAPTER_IMAGE_BYTES, jpeg_edge));
        let jpeg_total = MAX_CHAPTER_IMAGE_BYTES + jpeg_edge + IMAGE_DECODE_SCRATCH_BYTES;
        assert!(
            image_heap_room() - jpeg_total >= 128 * 1024,
            "jpeg edge leaves less than 128 KiB"
        );
        // Floyd-Steinberg runs after the file and the decoder planes are gone.
        let dither = (IMAGE_TARGET_WIDTH as usize) * (IMAGE_TARGET_HEIGHT as usize) * 3;
        assert!(dither + IMAGE_DECODE_SCRATCH_BYTES <= image_heap_room());
        assert!(jpeg_total <= image_heap_room());
        assert!(image_heap_room() < MODULE_PSRAM_BYTES);
        assert_eq!(MAX_CHAPTER_IMAGE_BYTES, 1024 * 1024);
        assert!(MAX_CHAPTER_IMAGES <= 8);
        assert!(IMAGE_DECODE_STACK_BYTES >= 48 * 1024);
    }
}
