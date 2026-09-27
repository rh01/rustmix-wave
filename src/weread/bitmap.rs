//! Bounded JPEG/PNG decode into a 1-bit bitmap for the e-paper panel.

use std::io::Cursor;

use image::{imageops::FilterType, DynamicImage, GenericImageView, ImageFormat};

use crate::weread::limits::{MAX_IMAGE_BYTES, MAX_IMAGE_EDGE};

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

pub fn decode_mono(
    bytes: &[u8],
    max_width: u32,
    max_height: u32,
) -> Result<MonoBitmap, &'static str> {
    if bytes.len() > MAX_IMAGE_BYTES || bytes.is_empty() {
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
    {
        return Err("image dimensions are out of range");
    }
    let image =
        image::load_from_memory_with_format(bytes, format).map_err(|_| "image decode failed")?;
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
    let scaled = if width > max_width || height > max_height {
        image.resize(max_width, max_height, FilterType::Nearest)
    } else {
        image
    };
    let gray = scaled.to_luma8();
    let (width, height) = gray.dimensions();
    if width == 0 || height == 0 || width > MAX_IMAGE_EDGE || height > MAX_IMAGE_EDGE {
        return Err("image dimensions are out of range");
    }
    let stride = (width as usize + 7) / 8;
    let mut packed = vec![0u8; stride * height as usize];
    for y in 0..height {
        for x in 0..width {
            let luma = gray.get_pixel(x, y)[0];
            if luma < 160 {
                let index = y as usize * stride + x as usize / 8;
                let shift = 7 - (x % 8);
                packed[index] |= 1 << shift;
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
    use super::{allowed_asset_url, decode_mono};
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
