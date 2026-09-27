//! Bounded JPEG/PNG decode into a 1-bit bitmap for the e-paper panel.
//!
//! The decoded pixel buffer is a heap `Vec`. On device, allocations larger than
//! 16 KiB (`CONFIG_SPIRAM_MALLOC_ALWAYSINTERNAL`) come from PSRAM, so a chapter
//! image is not taken from the internal heap. The SSD1677 driver has no
//! grayscale waveform ([`crate::panel_refresh::supports_grayscale_refresh`]),
//! so the bitmap is Floyd-Steinberg dithered to black and white.

use std::io::Cursor;

use image::{imageops::FilterType, DynamicImage, GenericImageView, ImageFormat};

use crate::{
    panel_refresh::supports_grayscale_refresh,
    weread::limits::{
        MAX_CHAPTER_IMAGE_BYTES, MAX_DECODE_PIXELS, MAX_IMAGE_DECODE_BYTES, MAX_IMAGE_EDGE,
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

pub fn decode_mono(
    bytes: &[u8],
    max_width: u32,
    max_height: u32,
) -> Result<MonoBitmap, &'static str> {
    if bytes.is_empty() || bytes.len() > MAX_CHAPTER_IMAGE_BYTES {
        return Err("image exceeds the size limit");
    }
    let format = image::guess_format(bytes).map_err(|_| "image format is not png or jpeg")?;
    if !matches!(format, ImageFormat::Png | ImageFormat::Jpeg) {
        return Err("image format is not png or jpeg");
    }
    let dimensions = image::io::Reader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| "image header is unreadable")?
        .into_dimensions()
        .map_err(|_| "image header is unreadable")?;
    if dimensions.0 == 0
        || dimensions.1 == 0
        || dimensions.0 > MAX_IMAGE_EDGE
        || dimensions.1 > MAX_IMAGE_EDGE
        || dimensions.0.saturating_mul(dimensions.1) > MAX_DECODE_PIXELS
    {
        return Err("image is too large");
    }
    let mut reader = image::io::Reader::with_format(Cursor::new(bytes), format);
    let mut limits = image::io::Limits::default();
    limits.max_alloc = Some(MAX_IMAGE_DECODE_BYTES as u64);
    limits.max_image_width = Some(MAX_IMAGE_EDGE);
    limits.max_image_height = Some(MAX_IMAGE_EDGE);
    reader.limits(limits);
    let image = reader.decode().map_err(|_| "image decode failed")?;
    rasterize(
        image,
        max_width.max(1).min(MAX_IMAGE_EDGE),
        max_height.max(1).min(MAX_IMAGE_EDGE),
    )
}

fn rasterize(
    image: DynamicImage,
    max_width: u32,
    max_height: u32,
) -> Result<MonoBitmap, &'static str> {
    let (width, height) = image.dimensions();
    let (target_w, target_h) = fitted_size(width, height, max_width, max_height);
    let scaled = if u32::from(target_w) != width || u32::from(target_h) != height {
        image.resize(
            u32::from(target_w),
            u32::from(target_h),
            FilterType::Triangle,
        )
    } else {
        image
    };
    let gray = scaled.to_luma8();
    let (width, height) = gray.dimensions();
    if width == 0 || height == 0 || width > MAX_IMAGE_EDGE || height > MAX_IMAGE_EDGE {
        return Err("image dimensions are out of range");
    }
    // 1-bit Floyd-Steinberg. A grayscale refresh is not available on this panel.
    let _ = supports_grayscale_refresh();
    Ok(floyd_steinberg(&gray))
}

fn floyd_steinberg(gray: &image::GrayImage) -> MonoBitmap {
    let width = gray.width() as usize;
    let height = gray.height() as usize;
    let mut luma = Vec::with_capacity(width.saturating_mul(height));
    luma.extend(gray.as_raw().iter().map(|pixel| i16::from(*pixel)));
    let stride = (width + 7) / 8;
    let mut packed = vec![0u8; stride.saturating_mul(height)];
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
    MonoBitmap {
        width: width as u16,
        height: height as u16,
        packed,
    }
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
    use image::{codecs::png::PngEncoder, GrayImage, ImageEncoder, Luma};
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
        let bitmap = decode_mono(&bytes, 48, 64).unwrap();
        assert!(bitmap.bit(0, 0));
        assert!(!bitmap.bit(1, 0));
        assert!(decode_mono(&huge_png_header(), 48, 64).is_err());
        assert!(!allowed_asset_url("http://weread.qq.com/a.jpg"));
        assert!(!allowed_asset_url("https://evil.example/a.jpg"));
        assert!(allowed_asset_url("https://res.weread.qq.com/a.jpg"));
        assert!(allowed_asset_url(
            "https://wfqqreader-1252317822.image.myqcloud.com/cover.jpg"
        ));
    }

    #[test]
    fn mid_gray_dithers_to_a_mix_of_black_and_white() {
        let image = GrayImage::from_pixel(4, 2, Luma([128]));
        let mut bytes = Vec::new();
        PngEncoder::new(Cursor::new(&mut bytes))
            .write_image(image.as_raw(), 4, 2, image::ColorType::L8)
            .unwrap();
        let bitmap = decode_mono(&bytes, 48, 64).unwrap();
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
