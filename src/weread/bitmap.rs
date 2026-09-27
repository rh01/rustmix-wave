//! Bounded JPEG/PNG decode into a 1-bit bitmap for the e-paper panel.
//!
//! The compressed bytes are dropped as soon as the decoder returns an owned
//! image, and that image is dropped as soon as a luma plane exists. Scaling
//! and Floyd-Steinberg then run on the luma plane only. Every buffer this
//! module owns is reserved with [`Vec::try_reserve_exact`]; a failure becomes
//! a placeholder instead of an abort. Decode itself runs on the `weread-img`
//! PSRAM stack, not the 16 KiB main task.
//!
//! The SSD1677 driver has no grayscale waveform
//! ([`crate::panel_refresh::supports_grayscale_refresh`]), so the bitmap is
//! dithered to black and white.

use std::io::Cursor;

use image::{DynamicImage, GenericImageView, ImageFormat};

use crate::{
    panel_refresh::supports_grayscale_refresh,
    runtime_worker::NamedWorkerHandle,
    weread::limits::{
        IMAGE_DECODE_STACK_BYTES, MAX_CHAPTER_IMAGE_BYTES, MAX_DECODE_PIXELS,
        MAX_IMAGE_DECODE_BYTES, MAX_IMAGE_EDGE,
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
/// `bytes` is dropped before the luma plane is allocated, so the compressed
/// file and the second pixel buffer do not coexist.
pub fn decode_mono(
    bytes: Vec<u8>,
    max_width: u32,
    max_height: u32,
) -> Result<MonoBitmap, &'static str> {
    if bytes.is_empty() || bytes.len() > MAX_CHAPTER_IMAGE_BYTES {
        return Err("image exceeds the size limit");
    }
    let format = image::guess_format(&bytes).map_err(|_| "image format is not png or jpeg")?;
    if !matches!(format, ImageFormat::Png | ImageFormat::Jpeg) {
        return Err("image format is not png or jpeg");
    }
    let (width, height) = image::io::Reader::new(Cursor::new(bytes.as_slice()))
        .with_guessed_format()
        .map_err(|_| "image header is unreadable")?
        .into_dimensions()
        .map_err(|_| "image header is unreadable")?;
    if !dimensions_allowed(width, height) {
        return Err("image is too large");
    }
    let pixels = (width as usize).saturating_mul(height as usize);
    // Prove the decoded buffer fits while the file is still held, then free
    // the probe before the decoder allocates its own copy.
    {
        let mut probe = Vec::<u8>::new();
        probe
            .try_reserve_exact(pixels.saturating_mul(4))
            .map_err(|_| "image decode failed")?;
    }
    let image = decode_dynamic(&bytes, format)?;
    drop(bytes);
    let (luma, width, height) = luma_plane(image)?;
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

fn dimensions_allowed(width: u32, height: u32) -> bool {
    width > 0
        && height > 0
        && width <= MAX_IMAGE_EDGE
        && height <= MAX_IMAGE_EDGE
        && u64::from(width) * u64::from(height) <= u64::from(MAX_DECODE_PIXELS)
}

fn decode_dynamic(bytes: &[u8], format: ImageFormat) -> Result<DynamicImage, &'static str> {
    let mut reader = image::io::Reader::with_format(Cursor::new(bytes), format);
    let mut limits = image::io::Limits::default();
    limits.max_alloc = Some(MAX_IMAGE_DECODE_BYTES as u64);
    limits.max_image_width = Some(MAX_IMAGE_EDGE);
    limits.max_image_height = Some(MAX_IMAGE_EDGE);
    reader.limits(limits);
    reader.decode().map_err(|_| "image decode failed")
}

/// One luma byte per pixel. The source image is dropped before this returns.
fn luma_plane(image: DynamicImage) -> Result<(Vec<u8>, u32, u32), &'static str> {
    let (width, height) = image.dimensions();
    if !dimensions_allowed(width, height) {
        return Err("image is too large");
    }
    let count = (width as usize).saturating_mul(height as usize);
    let mut luma = Vec::new();
    luma.try_reserve_exact(count)
        .map_err(|_| "image decode failed")?;
    match image {
        DynamicImage::ImageLuma8(image) => {
            luma.extend_from_slice(image.as_raw());
        }
        DynamicImage::ImageLumaA8(image) => {
            for pixel in image.pixels() {
                luma.push(pixel.0[0]);
            }
        }
        DynamicImage::ImageRgb8(image) => {
            for pixel in image.pixels() {
                luma.push(rec601(pixel.0[0], pixel.0[1], pixel.0[2]));
            }
        }
        DynamicImage::ImageRgba8(image) => {
            for pixel in image.pixels() {
                luma.push(rec601(pixel.0[0], pixel.0[1], pixel.0[2]));
            }
        }
        _ => return Err("image decode failed"),
    }
    if luma.len() != count {
        return Err("image decode failed");
    }
    Ok((luma, width, height))
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
        let bitmap = decode_mono(tiny_jpeg(), 48, 64).unwrap();
        assert!(bitmap.width >= 1 && bitmap.height >= 1);
        let bitmap = decode_mono(tiny_png(), 48, 64).unwrap();
        assert_eq!((bitmap.width, bitmap.height), (2, 2));
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
        let image = RgbImage::from_pixel(2, 2, image::Rgb([20, 40, 60]));
        let mut bytes = Vec::new();
        JpegEncoder::new(&mut bytes)
            .write_image(image.as_raw(), 2, 2, image::ColorType::Rgb8)
            .unwrap();
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
