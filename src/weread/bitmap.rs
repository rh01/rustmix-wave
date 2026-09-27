//! Bounded JPEG/PNG decode into a 1-bit bitmap for the e-paper panel.
//!
//! JPEG goes through `jpeg-decoder` and PNG through `png`, not `image`'s
//! `DynamicImage` path. That path allocates the output buffer before the
//! decoder's own planes, so a baseline JPEG briefly holds the file, the
//! component planes, and two full frames. Here the file is dropped as soon as
//! those planes are gone, and scaling plus Floyd-Steinberg run on a luma
//! plane only. Every buffer this module owns is reserved with
//! [`Vec::try_reserve_exact`]; a failure becomes a placeholder instead of an
//! abort. Decode itself runs on the `weread-img` PSRAM stack, not the 16 KiB
//! main task.
//!
//! The SSD1677 driver has no grayscale waveform
//! ([`crate::panel_refresh::supports_grayscale_refresh`]), so the bitmap is
//! dithered to black and white.

use std::io::Cursor;

use jpeg_decoder::{CodingProcess, Decoder as JpegDecoder, PixelFormat};
use png::{BitDepth, ColorType, Decoder as PngDecoder, Transformations};

use crate::{
    panel_refresh::supports_grayscale_refresh,
    runtime_worker::NamedWorkerHandle,
    weread::limits::{
        image_alloc_fits, jpeg_decoder_extra_bytes, jpeg_scaled_edge, IMAGE_DECODE_SCRATCH_BYTES,
        IMAGE_DECODE_STACK_BYTES, MAX_CHAPTER_IMAGE_BYTES, MAX_IMAGE_DECODE_BYTES, MAX_IMAGE_EDGE,
        MAX_JPEG_EDGE,
    },
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonoBitmap {
    pub width: u16,
    pub height: u16,
    packed: Vec<u8>,
}

impl MonoBitmap {
    #[must_use]
    pub fn bit(&self, x: u16, y: u16) -> bool {
        if x >= self.width || y >= self.height {
            return false;
        }
        let stride = (usize::from(self.width) + 7) / 8;
        let index = usize::from(y) * stride + usize::from(x) / 8;
        let shift = 7 - (x % 8);
        self.packed
            .get(index)
            .is_some_and(|byte| byte & (1 << shift) != 0)
    }
}

/// Scale `width`×`height` so both edges fit. Aspect ratio is kept.
#[must_use]
pub fn fitted_size(width: u32, height: u32, max_width: u32, max_height: u32) -> (u16, u16) {
    let max_width = max_width.max(1);
    let max_height = max_height.max(1);
    if width == 0 || height == 0 {
        return (1, 1);
    }
    let mut w = width;
    let mut h = height;
    if w > max_width {
        h = h.saturating_mul(max_width) / w.max(1);
        w = max_width;
    }
    if h > max_height {
        w = w.saturating_mul(max_height) / h.max(1);
        h = max_height;
    }
    if w == 0 {
        w = 1;
    }
    if h == 0 {
        h = 1;
    }
    (
        w.min(u32::from(u16::MAX)) as u16,
        h.min(u32::from(u16::MAX)) as u16,
    )
}

/// Decode one image the caller already owns.
///
/// `bytes` is dropped before the luma plane is allocated. JPEG scaling is
/// chosen so the decoder's own planes fit in [`image_heap_room`](crate::weread::limits::image_heap_room).
pub fn decode_mono(
    bytes: Vec<u8>,
    max_width: u32,
    max_height: u32,
) -> Result<MonoBitmap, &'static str> {
    if bytes.is_empty() || bytes.len() > MAX_CHAPTER_IMAGE_BYTES {
        return Err("image exceeds the size limit");
    }
    let (luma, width, height) = match sniff(&bytes) {
        Some(ImageKind::Jpeg) => {
            let (pixels, width, height, format) = decode_jpeg(&bytes)?;
            drop(bytes);
            let luma = match format {
                PixelFormat::CMYK32 => luma_from_cmyk(&pixels, width, height)?,
                _ => luma_from_rgb_like(&pixels, width, height, format.pixel_bytes())?,
            };
            (luma, width, height)
        }
        Some(ImageKind::Png) => {
            let (pixels, width, height, bpp) = decode_png(&bytes)?;
            drop(bytes);
            let luma = luma_from_rgb_like(&pixels, width, height, bpp)?;
            (luma, width, height)
        }
        None => return Err("image format is not png or jpeg"),
    };
    let (target_w, target_h) = fitted_size(
        width,
        height,
        max_width.max(1).min(MAX_IMAGE_EDGE),
        max_height.max(1).min(MAX_IMAGE_EDGE),
    );
    let scaled = scale_luma(
        &luma,
        width,
        height,
        u32::from(target_w),
        u32::from(target_h),
    )?;
    drop(luma);
    // 1-bit Floyd-Steinberg. A grayscale refresh is not available on this panel.
    let _ = supports_grayscale_refresh();
    floyd_steinberg(&scaled, u32::from(target_w), u32::from(target_h))
}

#[derive(Clone, Copy)]
enum ImageKind {
    Jpeg,
    Png,
}

fn sniff(bytes: &[u8]) -> Option<ImageKind> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n']) {
        Some(ImageKind::Png)
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(ImageKind::Jpeg)
    } else {
        None
    }
}

/// Decode on the `weread-img` thread. The main task only moves `bytes` in.
pub(crate) fn spawn_image_decode(
    bytes: Vec<u8>,
    max_width: u32,
    max_height: u32,
) -> std::io::Result<NamedWorkerHandle<Option<MonoBitmap>, &'static str>> {
    #[cfg(target_os = "espidf")]
    let _psram = crate::weread::http::psram_stack(IMAGE_DECODE_STACK_BYTES, c"weread-img");
    NamedWorkerHandle::spawn("weread-img", IMAGE_DECODE_STACK_BYTES, move || {
        Ok(decode_mono(bytes, max_width, max_height).ok())
    })
}

fn decode_jpeg(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32, PixelFormat), &'static str> {
    let mut decoder = JpegDecoder::new(Cursor::new(bytes));
    decoder
        .read_info()
        .map_err(|_| "image header is unreadable")?;
    let info = decoder.info().ok_or("image header is unreadable")?;
    if info.coding_process == CodingProcess::Lossless {
        return Err("image decode failed");
    }
    let components = match info.pixel_format {
        PixelFormat::L8 => 1,
        PixelFormat::RGB24 => 3,
        PixelFormat::CMYK32 => 4,
        PixelFormat::L16 => return Err("image decode failed"),
    };
    let progressive = info.coding_process == CodingProcess::DctProgressive;
    let src_w = info.width;
    let src_h = info.height;
    let (ask_w, ask_h) = select_jpeg_scale(src_w, src_h, components, progressive, bytes.len())?;
    let (got_w, got_h) = decoder
        .scale(ask_w, ask_h)
        .map_err(|_| "image is too large")?;
    if u32::from(got_w) > MAX_JPEG_EDGE || u32::from(got_h) > MAX_JPEG_EDGE {
        return Err("image is too large");
    }
    let extra = jpeg_decoder_extra_bytes(
        src_w,
        src_h,
        u32::from(got_w),
        u32::from(got_h),
        components,
        progressive,
    )
    .ok_or("image is too large")?;
    if !image_alloc_fits(bytes.len(), extra) {
        return Err("image is too large");
    }
    // The decoder's planes are infallible allocations. Reserve their peak
    // first so a shortfall becomes a placeholder, then free it before decode.
    {
        let mut probe = Vec::<u8>::new();
        probe
            .try_reserve_exact(extra.saturating_add(IMAGE_DECODE_SCRATCH_BYTES))
            .map_err(|_| "image decode failed")?;
    }
    let pixels = decoder.decode().map_err(|_| "image decode failed")?;
    let width = u32::from(got_w);
    let height = u32::from(got_h);
    let expect = (width as usize)
        .checked_mul(height as usize)
        .and_then(|count| count.checked_mul(info.pixel_format.pixel_bytes()))
        .ok_or("image is too large")?;
    if pixels.len() != expect {
        return Err("image decode failed");
    }
    Ok((pixels, width, height, info.pixel_format))
}

fn select_jpeg_scale(
    src_w: u16,
    src_h: u16,
    components: usize,
    progressive: bool,
    file_len: usize,
) -> Result<(u16, u16), &'static str> {
    for idct in [8u32, 4, 2, 1] {
        let out_w = jpeg_scaled_edge(src_w, idct);
        let out_h = jpeg_scaled_edge(src_h, idct);
        if out_w == 0 || out_h == 0 || out_w > MAX_JPEG_EDGE || out_h > MAX_JPEG_EDGE {
            continue;
        }
        let Some(extra) =
            jpeg_decoder_extra_bytes(src_w, src_h, out_w, out_h, components, progressive)
        else {
            continue;
        };
        if !image_alloc_fits(file_len, extra) {
            continue;
        }
        let out_w = u16::try_from(out_w).map_err(|_| "image is too large")?;
        let out_h = u16::try_from(out_h).map_err(|_| "image is too large")?;
        return Ok((out_w, out_h));
    }
    Err("image is too large")
}

fn decode_png(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32, usize), &'static str> {
    let mut limits = png::Limits::default();
    limits.bytes = IMAGE_DECODE_SCRATCH_BYTES;
    let mut decoder = PngDecoder::new_with_limits(Cursor::new(bytes), limits);
    decoder.set_transformations(Transformations::EXPAND | Transformations::STRIP_16);
    let mut reader = decoder
        .read_info()
        .map_err(|_| "image header is unreadable")?;
    let (width, height) = reader.info().size();
    if width == 0
        || height == 0
        || width > MAX_IMAGE_EDGE
        || height > MAX_IMAGE_EDGE
        || !image_alloc_fits(
            bytes.len(),
            (width as usize).saturating_mul(height as usize) * 4,
        )
    {
        return Err("image is too large");
    }
    let output = reader.output_buffer_size();
    if output > MAX_IMAGE_DECODE_BYTES || !image_alloc_fits(bytes.len(), output) {
        return Err("image is too large");
    }
    let mut pixels = Vec::new();
    pixels
        .try_reserve_exact(output)
        .map_err(|_| "image decode failed")?;
    pixels.resize(output, 0);
    let (color, depth) = reader.output_color_type();
    reader
        .next_frame(&mut pixels)
        .map_err(|_| "image decode failed")?;
    if depth != BitDepth::Eight {
        return Err("image decode failed");
    }
    let bpp = match color {
        ColorType::Grayscale => 1,
        ColorType::GrayscaleAlpha => 2,
        ColorType::Rgb => 3,
        ColorType::Rgba => 4,
        ColorType::Indexed => return Err("image decode failed"),
    };
    Ok((pixels, width, height, bpp))
}

fn luma_from_rgb_like(
    pixels: &[u8],
    width: u32,
    height: u32,
    bpp: usize,
) -> Result<Vec<u8>, &'static str> {
    let count = (width as usize)
        .checked_mul(height as usize)
        .ok_or("image is too large")?;
    if bpp == 0 || pixels.len() != count.saturating_mul(bpp) {
        return Err("image decode failed");
    }
    let mut luma = Vec::new();
    luma.try_reserve_exact(count)
        .map_err(|_| "image decode failed")?;
    match bpp {
        1 => luma.extend_from_slice(pixels),
        2 => {
            for pixel in pixels.chunks_exact(2) {
                luma.push(pixel[0]);
            }
        }
        3 => {
            for pixel in pixels.chunks_exact(3) {
                luma.push(rec601(pixel[0], pixel[1], pixel[2]));
            }
        }
        4 => {
            for pixel in pixels.chunks_exact(4) {
                luma.push(rec601(pixel[0], pixel[1], pixel[2]));
            }
        }
        _ => return Err("image decode failed"),
    }
    if luma.len() != count {
        return Err("image decode failed");
    }
    Ok(luma)
}

fn luma_from_cmyk(pixels: &[u8], width: u32, height: u32) -> Result<Vec<u8>, &'static str> {
    let count = (width as usize)
        .checked_mul(height as usize)
        .ok_or("image is too large")?;
    if pixels.len() != count.saturating_mul(4) {
        return Err("image decode failed");
    }
    let mut luma = Vec::new();
    luma.try_reserve_exact(count)
        .map_err(|_| "image decode failed")?;
    for pixel in pixels.chunks_exact(4) {
        let cyan = 255 - u16::from(pixel[0]);
        let magenta = 255 - u16::from(pixel[1]);
        let yellow = 255 - u16::from(pixel[2]);
        let black = 255 - u16::from(pixel[3]);
        luma.push(rec601(
            ((black * cyan) / 255) as u8,
            ((black * magenta) / 255) as u8,
            ((black * yellow) / 255) as u8,
        ));
    }
    Ok(luma)
}

fn rec601(red: u8, green: u8, blue: u8) -> u8 {
    ((u32::from(red) * 2126 + u32::from(green) * 7152 + u32::from(blue) * 722) / 10_000) as u8
}

/// Area-average into the destination only. The source stays borrowed.
fn scale_luma(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
) -> Result<Vec<u8>, &'static str> {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return Err("image dimensions are out of range");
    }
    let count = (dst_w as usize).saturating_mul(dst_h as usize);
    let mut dest = Vec::new();
    dest.try_reserve_exact(count)
        .map_err(|_| "image decode failed")?;
    if src_w == dst_w && src_h == dst_h {
        if src.len() != count {
            return Err("image decode failed");
        }
        dest.extend_from_slice(src);
        return Ok(dest);
    }
    for y in 0..dst_h {
        let y0 = ((u64::from(y) * u64::from(src_h)) / u64::from(dst_h)) as u32;
        let y1 = ((u64::from(y + 1) * u64::from(src_h)) / u64::from(dst_h)) as u32;
        let y1 = y1.max(y0 + 1).min(src_h);
        for x in 0..dst_w {
            let x0 = ((u64::from(x) * u64::from(src_w)) / u64::from(dst_w)) as u32;
            let x1 = ((u64::from(x + 1) * u64::from(src_w)) / u64::from(dst_w)) as u32;
            let x1 = x1.max(x0 + 1).min(src_w);
            let mut sum = 0u32;
            let mut samples = 0u32;
            for yy in y0..y1 {
                let row = (yy as usize) * (src_w as usize);
                for xx in x0..x1 {
                    sum += u32::from(src[row + xx as usize]);
                    samples += 1;
                }
            }
            dest.push((sum / samples.max(1)) as u8);
        }
    }
    Ok(dest)
}

fn floyd_steinberg(gray: &[u8], width: u32, height: u32) -> Result<MonoBitmap, &'static str> {
    let width = width as usize;
    let height = height as usize;
    if width == 0 || height == 0 || gray.len() != width.saturating_mul(height) {
        return Err("image dimensions are out of range");
    }
    let mut luma = Vec::new();
    luma.try_reserve_exact(gray.len())
        .map_err(|_| "image decode failed")?;
    luma.extend(gray.iter().map(|pixel| i16::from(*pixel)));
    let stride = (width + 7) / 8;
    let packed_len = stride.saturating_mul(height);
    let mut packed = Vec::new();
    packed
        .try_reserve_exact(packed_len)
        .map_err(|_| "image decode failed")?;
    packed.resize(packed_len, 0);
    for y in 0..height {
        for x in 0..width {
            let index = y * width + x;
            let old = luma[index].clamp(0, 255);
            let new: i16 = if old < 128 { 0 } else { 255 };
            if new == 0 {
                let byte = y * stride + x / 8;
                let shift = 7 - (x % 8);
                packed[byte] |= 1 << shift;
            }
            let error = old - new;
            if x + 1 < width {
                luma[index + 1] += error * 7 / 16;
            }
            if y + 1 < height {
                if x > 0 {
                    luma[index + width - 1] += error * 3 / 16;
                }
                luma[index + width] += error * 5 / 16;
                if x + 1 < width {
                    luma[index + width + 1] += error / 16;
                }
            }
        }
    }
    Ok(MonoBitmap {
        width: width as u16,
        height: height as u16,
        packed,
    })
}

#[must_use]
pub fn allowed_asset_url(url: &str) -> bool {
    if url.len() > crate::weread::limits::MAX_URL_CHARS || !url.starts_with("https://") {
        return false;
    }
    let host = url
        .trim_start_matches("https://")
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    host == "weread.qq.com"
        || host.ends_with(".weread.qq.com")
        || host.ends_with(".qq.com")
        || host.ends_with(".myqcloud.com")
}

#[cfg(test)]
mod tests {
    use super::{allowed_asset_url, decode_mono, fitted_size};
    use image::{
        codecs::{jpeg::JpegEncoder, png::PngEncoder},
        GrayImage, ImageEncoder, Luma, RgbImage,
    };
    use std::io::Cursor;

    #[test]
    fn png_threshold_and_hostile_dimensions() {
        let mut image = GrayImage::new(2, 1);
        image.put_pixel(0, 0, Luma([0]));
        image.put_pixel(1, 0, Luma([255]));
        let mut bytes = Vec::new();
        PngEncoder::new(Cursor::new(&mut bytes))
            .write_image(image.as_raw(), 2, 1, image::ColorType::L8)
            .unwrap();
        let bitmap = decode_mono(bytes, 48, 64).unwrap();
        assert!(bitmap.bit(0, 0));
        assert!(!bitmap.bit(1, 0));
        assert!(decode_mono(huge_png_header(), 48, 64).is_err());
        assert!(!allowed_asset_url("http://weread.qq.com/a.jpg"));
        assert!(!allowed_asset_url("https://evil.example/a.jpg"));
        assert!(allowed_asset_url("https://res.weread.qq.com/a.jpg"));
        assert!(allowed_asset_url(
            "https://wfqqreader-1252317822.image.myqcloud.com/cover.jpg"
        ));
    }

    #[test]
    fn corrupt_truncated_and_oversize_images_are_rejected() {
        assert!(decode_mono(Vec::new(), 48, 64).is_err());
        assert!(decode_mono(vec![0xFF, 0xD8, 0xFF, 0xD9], 48, 64).is_err());
        let jpeg = tiny_jpeg();
        assert!(decode_mono(jpeg[..jpeg.len() / 2].to_vec(), 48, 64).is_err());
        let png = tiny_png();
        assert!(decode_mono(png[..png.len() / 2].to_vec(), 48, 64).is_err());
        assert!(decode_mono(huge_png_header(), 48, 64).is_err());
        assert!(decode_mono(jpeg_sof(8_000, 8_000), 752, 594).is_err());
        let bitmap = decode_mono(tiny_jpeg(), 48, 64).unwrap();
        assert!(bitmap.width >= 1 && bitmap.height >= 1);
        let bitmap = decode_mono(tiny_png(), 48, 64).unwrap();
        assert_eq!((bitmap.width, bitmap.height), (2, 2));
    }

    #[test]
    fn wide_baseline_jpeg_is_scaled_inside_the_decoder_budget() {
        let bitmap = decode_mono(solid_jpeg(800, 800), 752, 594).unwrap();
        assert!(
            (300..=592).contains(&bitmap.width) && (300..=592).contains(&bitmap.height),
            "decoded {}x{}, full 800px planes would exceed PSRAM",
            bitmap.width,
            bitmap.height
        );
    }

    #[test]
    fn mid_gray_dithers_to_a_mix_of_black_and_white() {
        let image = GrayImage::from_pixel(4, 2, Luma([128]));
        let mut bytes = Vec::new();
        PngEncoder::new(Cursor::new(&mut bytes))
            .write_image(image.as_raw(), 4, 2, image::ColorType::L8)
            .unwrap();
        let bitmap = decode_mono(bytes, 48, 64).unwrap();
        let mut black = 0;
        for y in 0..bitmap.height {
            for x in 0..bitmap.width {
                if bitmap.bit(x, y) {
                    black += 1;
                }
            }
        }
        assert!(black > 0 && black < 8, "black pixels {black}");
    }

    #[test]
    fn fitted_size_keeps_aspect_inside_the_page() {
        assert_eq!(fitted_size(800, 400, 400, 600), (400, 200));
        assert_eq!(fitted_size(200, 800, 400, 300), (75, 300));
        assert_eq!(fitted_size(40, 20, 400, 300), (40, 20));
    }

    fn tiny_png() -> Vec<u8> {
        let image = GrayImage::from_pixel(2, 2, Luma([0]));
        let mut bytes = Vec::new();
        PngEncoder::new(Cursor::new(&mut bytes))
            .write_image(image.as_raw(), 2, 2, image::ColorType::L8)
            .unwrap();
        bytes
    }

    fn tiny_jpeg() -> Vec<u8> {
        solid_jpeg(2, 2)
    }

    fn solid_jpeg(width: u32, height: u32) -> Vec<u8> {
        let image = RgbImage::from_pixel(width, height, image::Rgb([20, 40, 60]));
        let mut bytes = Vec::new();
        JpegEncoder::new(&mut bytes)
            .write_image(image.as_raw(), width, height, image::ColorType::Rgb8)
            .unwrap();
        bytes
    }

    /// SOF-only JPEG. `read_info` succeeds and the dimension check must reject
    /// it before `decode` allocates coefficient or plane buffers.
    fn jpeg_sof(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xC0];
        bytes.extend(17u16.to_be_bytes());
        bytes.push(8);
        bytes.extend(height.to_be_bytes());
        bytes.extend(width.to_be_bytes());
        bytes.push(3);
        for id in 1..=3 {
            bytes.push(id);
            bytes.push(0x11);
            bytes.push(0);
        }
        bytes
    }

    fn huge_png_header() -> Vec<u8> {
        let mut bytes = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&100_000u32.to_be_bytes());
        bytes.extend_from_slice(&100_000u32.to_be_bytes());
        bytes.extend_from_slice(&[8, 0, 0, 0, 0]);
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes
    }
}
