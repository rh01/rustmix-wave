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
/// PNG source edge. Larger headers are a placeholder.
///
/// Chosen so a 1 MiB file, one RGBA buffer, and the 256 KiB zlib window still
/// fit beside the resident buffers, including seven bitmaps already upscaled
/// to the reader content box, with [`IMAGE_SAFETY_MARGIN_BYTES`] left over.
pub const MAX_IMAGE_EDGE: u32 = 192;
/// Pixel count accepted for one PNG.
pub const MAX_DECODE_PIXELS: u32 = MAX_IMAGE_EDGE * MAX_IMAGE_EDGE;
/// One PNG output buffer (RGBA, 4 bytes per pixel).
pub const MAX_IMAGE_DECODE_BYTES: usize = (MAX_DECODE_PIXELS as usize) * 4;
/// JPEG output edge after in-decoder scaling (1, 1/2, 1/4, or 1/8).
///
/// Progressive coefficient planes stay at the full source size, so a large
/// progressive file is rejected even when the scaled output would be small.
pub const MAX_JPEG_EDGE: u32 = 256;
/// `png` grows `ZlibStream::out_buffer` by doubling until it reaches 256 KiB
/// (`LOOKBACK_SIZE * 4`). That allocation is infallible.
pub const PNG_ZLIB_OUT_BYTES: usize = 256 * 1024;
/// Headroom that must remain after the worst-case decode peak.
pub const IMAGE_SAFETY_MARGIN_BYTES: usize = 512 * 1024;
/// Widest reader measure. Decode stays at [`MAX_IMAGE_EDGE`] / [`MAX_JPEG_EDGE`];
/// the luma is upscaled to this box only after the file and coefficients are gone.
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

/// Packed 1-bit bytes of one bitmap that fills the reader content box.
#[must_use]
pub fn packed_bitmap_bytes(width: u32, height: u32) -> usize {
    let stride = (width as usize).saturating_add(7) / 8;
    stride.saturating_mul(height as usize)
}

/// Packed 1-bit bytes of the bitmaps already installed when the next image
/// starts. One slot is the image being decoded, so at most seven are live.
///
/// Stored bitmaps are the upscaled content size, not the decoder edge.
#[must_use]
pub fn loaded_chapter_bitmap_bytes() -> usize {
    packed_bitmap_bytes(IMAGE_TARGET_WIDTH, IMAGE_TARGET_HEIGHT)
        .saturating_mul(MAX_CHAPTER_IMAGES.saturating_sub(1))
}

/// Bytes already occupied when a chapter image is decoded.
///
/// The SD font, glyph cache, both WeRead stacks, both panel framebuffers, the
/// chapter string, its paginated copy, and up to seven installed bitmaps.
#[must_use]
pub fn image_resident_bytes() -> usize {
    crate::fonts::SD_FONT_RESIDENT_BUDGET_BYTES
        + crate::fonts::GLYPH_CACHE_BUDGET_BYTES
        + WEREAD_HTTP_WORKER_STACK_BYTES
        + IMAGE_DECODE_STACK_BYTES
        + crate::framebuffer::FRAMEBUFFER_SIZE * 2
        + MAX_CHAPTER_TEXT * 2
        + loaded_chapter_bitmap_bytes()
}

/// Room for the compressed file and the decoder after residents and the safety
/// margin. A peak above this is a placeholder; nothing infallible is allocated.
#[must_use]
pub fn image_heap_room() -> usize {
    MODULE_PSRAM_BYTES
        .saturating_sub(image_resident_bytes())
        .saturating_sub(IMAGE_SAFETY_MARGIN_BYTES)
}

/// File plus RGBA output plus the zlib window.
#[must_use]
pub fn png_decode_peak(file_len: usize, width: u32, height: u32) -> Option<usize> {
    let pixels = (width as usize).checked_mul(height as usize)?;
    let rgba = pixels.checked_mul(4)?;
    file_len.checked_add(rgba)?.checked_add(PNG_ZLIB_OUT_BYTES)
}

/// Output edge `jpeg-decoder` produces for `idct` (8 = full, 4 = 1/2, 2 = 1/4, 1 = 1/8).
#[must_use]
pub fn jpeg_scaled_edge(len: u16, idct: u32) -> u32 {
    u32::from(len).saturating_mul(idct).saturating_sub(1) / 8 + 1
}

/// Bytes `jpeg-decoder` allocates besides the compressed file.
///
/// Always counted together, because they overlap:
/// component planes, the interleaved frame, full-resolution progressive
/// coefficients (scaling does not shrink `block_size`), and MCU-row copies.
/// Vertical sampling is taken as 4. Progressive `.to_vec`s one row while the
/// coefficients are still live. Baseline `mem::replace` allocates the next row
/// before releasing the previous one, so baseline counts two rows.
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
        let blocks_x = usize::from(src_w).div_ceil(8);
        let blocks_y = usize::from(src_h).div_ceil(8);
        blocks_x
            .checked_mul(blocks_y)?
            .checked_mul(64)?
            .checked_mul(2)?
            .checked_mul(components)?
    } else {
        0
    };
    let mcu_row = usize::from(src_w)
        .div_ceil(8)
        .checked_mul(4)?
        .checked_mul(64)?
        .checked_mul(2)?
        .checked_mul(components)?;
    let rows = if progressive {
        mcu_row
    } else {
        mcu_row.checked_mul(2)?
    };
    planes
        .checked_add(interleaved)?
        .checked_add(coeff)?
        .checked_add(rows)
}

/// Largest single infallible `Vec` inside `jpeg-decoder` for this scale.
///
/// Coefficient planes, MCU rows, component planes, and the interleaved frame
/// are separate allocations. The check is the biggest one, not their sum.
#[must_use]
pub fn jpeg_largest_infallible(
    src_w: u16,
    src_h: u16,
    out_w: u32,
    out_h: u32,
    components: usize,
    progressive: bool,
) -> Option<usize> {
    let plane = (out_w as usize).checked_mul(out_h as usize)?;
    let interleaved = plane.checked_mul(components.max(1))?;
    let coeff = if progressive {
        usize::from(src_w)
            .div_ceil(8)
            .checked_mul(usize::from(src_h).div_ceil(8))?
            .checked_mul(64)?
            .checked_mul(2)?
    } else {
        0
    };
    let mcu_row = usize::from(src_w)
        .div_ceil(8)
        .checked_mul(4)?
        .checked_mul(64)?
        .checked_mul(2)?;
    Some(plane.max(interleaved).max(coeff).max(mcu_row))
}

/// Live PSRAM headroom. Host tests have no SPIRAM heap, so they report the
/// static budget as unbounded and [`image_alloc_fits`] keeps the formula check.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpiramHeadroom {
    pub free_bytes: usize,
    pub largest_block_bytes: usize,
}

#[must_use]
pub fn spiram_headroom() -> SpiramHeadroom {
    #[cfg(target_os = "espidf")]
    {
        use esp_idf_svc::sys;
        SpiramHeadroom {
            free_bytes: unsafe { sys::heap_caps_get_free_size(sys::MALLOC_CAP_SPIRAM as u32) },
            largest_block_bytes: unsafe {
                sys::heap_caps_get_largest_free_block(sys::MALLOC_CAP_SPIRAM as u32)
            },
        }
    }
    #[cfg(not(target_os = "espidf"))]
    {
        SpiramHeadroom {
            free_bytes: usize::MAX,
            largest_block_bytes: usize::MAX,
        }
    }
}

/// `upcoming` is what decode will still allocate. `largest_infallible` is one
/// contiguous buffer. Both must leave [`IMAGE_SAFETY_MARGIN_BYTES`] free.
#[must_use]
pub fn spiram_can_hold(
    free_bytes: usize,
    largest_block_bytes: usize,
    upcoming: usize,
    largest_infallible: usize,
) -> bool {
    free_bytes >= upcoming.saturating_add(IMAGE_SAFETY_MARGIN_BYTES)
        && largest_block_bytes >= largest_infallible.saturating_add(IMAGE_SAFETY_MARGIN_BYTES)
}

/// Static room plus the live SPIRAM check.
///
/// `peak` includes the compressed file (already allocated). `upcoming` does
/// not, so total free is not asked to hold the file twice. `largest_infallible`
/// is the biggest `vec!` still ahead.
#[must_use]
pub fn image_alloc_fits(peak: usize, upcoming: usize, largest_infallible: usize) -> bool {
    if peak > image_heap_room() {
        return false;
    }
    let heap = spiram_headroom();
    spiram_can_hold(
        heap.free_bytes,
        heap.largest_block_bytes,
        upcoming,
        largest_infallible,
    )
}

pub const WEREAD_ROOT: &str = "/sdcard/RUSTMIX/WEREAD";
pub const WEREAD_CONFIG_PATH: &str = "/sdcard/RUSTMIX/WEREAD.TXT";
pub const SESSION_FILE: &str = "SESS.TXT";
pub const SESSION_TMP: &str = "SESS.TMP";
pub const SESSION_BAK: &str = "SESS.BAK";

#[cfg(test)]
mod tests {
    use super::{
        image_alloc_fits, image_heap_room, image_resident_bytes, jpeg_decoder_extra_bytes,
        jpeg_largest_infallible, jpeg_scaled_edge, loaded_chapter_bitmap_bytes,
        packed_bitmap_bytes, png_decode_peak, spiram_can_hold, DOWNLOAD_CHUNK_BYTES,
        HTTP_CHAPTER_LIMIT_MS, HTTP_IDLE_LIMIT_MS, HTTP_IO_BUFFER_BYTES, HTTP_READ_TIMEOUT_MS,
        HTTP_TIMEOUT_SECS, IMAGE_DECODE_STACK_BYTES, IMAGE_SAFETY_MARGIN_BYTES,
        IMAGE_TARGET_HEIGHT, IMAGE_TARGET_WIDTH, MAX_CHAPTER_IMAGES, MAX_CHAPTER_IMAGE_BYTES,
        MAX_CHAPTER_TEXT, MAX_DECODE_PIXELS, MAX_IMAGE_DECODE_BYTES, MAX_IMAGE_EDGE, MAX_JPEG_EDGE,
        MAX_SHARD_BYTES, MODULE_PSRAM_BYTES, PNG_ZLIB_OUT_BYTES, WEREAD_HTTP_WORKER_STACK_BYTES,
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
        assert_eq!(PNG_ZLIB_OUT_BYTES, 256 * 1024);
        assert_eq!(IMAGE_SAFETY_MARGIN_BYTES, 512 * 1024);
        assert!(image_resident_bytes() > crate::framebuffer::FRAMEBUFFER_SIZE * 2);
        assert!(loaded_chapter_bitmap_bytes() > 0);
        assert!(
            image_heap_room() + IMAGE_SAFETY_MARGIN_BYTES + image_resident_bytes()
                <= MODULE_PSRAM_BYTES
        );
        let png_peak = png_decode_peak(MAX_CHAPTER_IMAGE_BYTES, MAX_IMAGE_EDGE, MAX_IMAGE_EDGE)
            .expect("png edge");
        let png_upcoming = png_peak - MAX_CHAPTER_IMAGE_BYTES;
        assert!(
            image_alloc_fits(png_peak, png_upcoming, PNG_ZLIB_OUT_BYTES),
            "png square at the edge must fit, peak {png_peak} room {}",
            image_heap_room()
        );
        assert!(!image_alloc_fits(
            png_decode_peak(MAX_CHAPTER_IMAGE_BYTES, 800, 800).unwrap_or(usize::MAX),
            0,
            0
        ));
        // File is dropped before luma, so RGBA + luma is a later, smaller peak.
        let png_and_luma = MAX_DECODE_PIXELS as usize * 5;
        assert!(png_and_luma <= image_heap_room());
        let full_800 = jpeg_decoder_extra_bytes(800, 800, 800, 800, 3, false).unwrap();
        assert!(!image_alloc_fits(
            MAX_CHAPTER_IMAGE_BYTES + full_800,
            full_800,
            0
        ));
        // Seven content-sized bitmaps leave no room for a 1/4-scale 800px JPEG.
        let quarter = jpeg_scaled_edge(800, 2);
        let quarter_800 = jpeg_decoder_extra_bytes(800, 800, quarter, quarter, 3, false).unwrap();
        assert!(!image_alloc_fits(
            MAX_CHAPTER_IMAGE_BYTES + quarter_800,
            quarter_800,
            0
        ));
        let eighth = jpeg_scaled_edge(800, 1);
        assert!(eighth <= MAX_JPEG_EDGE);
        let eighth_800 = jpeg_decoder_extra_bytes(800, 800, eighth, eighth, 3, false).unwrap();
        let eighth_largest = jpeg_largest_infallible(800, 800, eighth, eighth, 3, false).unwrap();
        assert!(image_alloc_fits(
            MAX_CHAPTER_IMAGE_BYTES + eighth_800,
            eighth_800,
            eighth_largest
        ));
        // Full-frame 4-component output at the JPEG cap does not fit a 1 MiB file.
        // Half of that cap does.
        let jpeg_edge = jpeg_decoder_extra_bytes(
            MAX_JPEG_EDGE as u16,
            MAX_JPEG_EDGE as u16,
            MAX_JPEG_EDGE,
            MAX_JPEG_EDGE,
            4,
            false,
        )
        .unwrap();
        assert!(!image_alloc_fits(
            MAX_CHAPTER_IMAGE_BYTES + jpeg_edge,
            jpeg_edge,
            0
        ));
        let half_edge = jpeg_scaled_edge(MAX_JPEG_EDGE as u16, 4);
        let half = jpeg_decoder_extra_bytes(
            MAX_JPEG_EDGE as u16,
            MAX_JPEG_EDGE as u16,
            half_edge,
            half_edge,
            4,
            false,
        )
        .unwrap();
        assert!(image_alloc_fits(MAX_CHAPTER_IMAGE_BYTES + half, half, half));
        // Progressive coefficients stay full-size and are added to the MCU-row copy.
        let wide = jpeg_decoder_extra_bytes(1600, 8, 8, 8, 3, true).unwrap();
        let coeff = 1600usize.div_ceil(8) * 8usize.div_ceil(8) * 64 * 2 * 3;
        let mcu = 1600usize.div_ceil(8) * 4 * 64 * 2 * 3;
        assert!(mcu >= 300 * 1024, "wide vertical-4 row is {mcu}");
        assert!(wide >= coeff + mcu);
        let wide_largest = jpeg_largest_infallible(1600, 8, 8, 8, 3, true).unwrap();
        assert!(wide_largest >= coeff / 3, "one coefficient plane");
        assert!(!image_alloc_fits(
            MAX_CHAPTER_IMAGE_BYTES
                + jpeg_decoder_extra_bytes(800, 800, quarter, quarter, 3, true).unwrap(),
            0,
            0
        ));
        let display = (IMAGE_TARGET_WIDTH as usize) * (IMAGE_TARGET_HEIGHT as usize);
        assert!(
            display.saturating_mul(3) <= image_heap_room(),
            "upscale plus dither runs after the file is dropped"
        );
        assert!(
            loaded_chapter_bitmap_bytes()
                >= packed_bitmap_bytes(IMAGE_TARGET_WIDTH, IMAGE_TARGET_HEIGHT)
        );
        assert!(image_heap_room() < MODULE_PSRAM_BYTES);
        assert_eq!(MAX_CHAPTER_IMAGE_BYTES, 1024 * 1024);
        assert!(MAX_CHAPTER_IMAGES <= 8);
        assert!(IMAGE_DECODE_STACK_BYTES >= 48 * 1024);
        assert!(!spiram_can_hold(
            2_000_000,
            400_000,
            PNG_ZLIB_OUT_BYTES,
            PNG_ZLIB_OUT_BYTES
        ));
        assert!(spiram_can_hold(
            4_000_000,
            2_000_000,
            PNG_ZLIB_OUT_BYTES,
            PNG_ZLIB_OUT_BYTES
        ));
        assert!(!spiram_can_hold(
            PNG_ZLIB_OUT_BYTES,
            2_000_000,
            PNG_ZLIB_OUT_BYTES,
            PNG_ZLIB_OUT_BYTES
        ));
    }
}
