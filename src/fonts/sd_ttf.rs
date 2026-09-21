//! Optional SD-card TTF/OTF CJK faces rasterized with fontdue into PSRAM.

use std::{fs, path::Path};

use super::{RasterGlyph, SdFontFace};

/// Search order. `/fonts` is the documented card-root install path; the
/// product tree copy is convenient for the Wi-Fi transfer portal.
pub const SD_FONT_DIRECTORIES: [&str; 2] = ["/sdcard/fonts", "/sdcard/RUSTMIX/FONTS"];
const MAX_SD_FACES: usize = 4;
const MAX_SD_FONT_BYTES: usize = 2 * 1024 * 1024;

pub struct LoadedSdFace {
    pub info: SdFontFace,
    font: fontdue::Font,
}

impl LoadedSdFace {
    #[must_use]
    pub fn file_name(&self) -> &str {
        &self.info.file_name
    }

    #[must_use]
    pub fn rasterize(&self, character: char, px: u8) -> Option<RasterGlyph> {
        if self.font.lookup_glyph_index(character) == 0 && character != '\0' {
            return None;
        }
        let (metrics, coverage) = self.font.rasterize(character, f32::from(px));
        if metrics.width == 0 || metrics.height == 0 || coverage.is_empty() {
            return None;
        }
        pack_coverage(metrics, &coverage, px)
    }
}

pub fn scan_and_load(extra_roots: &[&Path]) -> (Vec<LoadedSdFace>, Vec<String>) {
    let mut notes = Vec::new();
    let mut faces = Vec::new();
    let mut roots: Vec<std::path::PathBuf> =
        extra_roots.iter().map(|path| path.to_path_buf()).collect();
    roots.extend(SD_FONT_DIRECTORIES.iter().map(std::path::PathBuf::from));
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) => {
                notes.push(format!("{}: {error}", root.display()));
                continue;
            }
        };
        let mut files: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
        files.sort();
        for path in files {
            if faces.len() >= MAX_SD_FACES {
                notes.push("sd-font-limit-reached".into());
                break;
            }
            if !is_open_font_path(&path) {
                continue;
            }
            match load_face(&path, faces.len() as u8) {
                Ok(face) => {
                    notes.push(format!(
                        "loaded {} bytes={}",
                        face.info.file_name, face.info.bytes
                    ));
                    faces.push(face);
                }
                Err(error) => notes.push(format!("{}: {error}", path.display())),
            }
        }
    }
    (faces, notes)
}

fn load_face(path: &Path, index: u8) -> Result<LoadedSdFace, String> {
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    if metadata.len() as usize > MAX_SD_FONT_BYTES {
        return Err(format!(
            "skipped ({} bytes > {MAX_SD_FONT_BYTES} PSRAM cap)",
            metadata.len()
        ));
    }
    let data = fs::read(path).map_err(|error| error.to_string())?;
    let font = fontdue::Font::from_bytes(data.as_slice(), fontdue::FontSettings::default())
        .map_err(|error| error.to_string())?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("FONT.TTF")
        .to_string();
    let label = file_name
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(file_name.as_str())
        .to_string();
    Ok(LoadedSdFace {
        info: SdFontFace {
            index,
            file_name,
            label,
            bytes: data.len(),
        },
        font,
    })
}

fn is_open_font_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| matches!(ext.to_ascii_lowercase().as_str(), "ttf" | "otf"))
        .unwrap_or(false)
}

fn pack_coverage(metrics: fontdue::Metrics, coverage: &[u8], px: u8) -> Option<RasterGlyph> {
    let width = u8::try_from(metrics.width.min(255)).ok()?;
    let height = u8::try_from(metrics.height.min(255)).ok()?;
    let stride = (usize::from(width) + 7) / 8;
    let mut bitmap = vec![0u8; stride * usize::from(height.max(1))];
    for y in 0..metrics.height {
        for x in 0..metrics.width {
            let alpha = coverage.get(y * metrics.width + x).copied().unwrap_or(0);
            if alpha < 96 {
                continue;
            }
            let byte_index = y * stride + x / 8;
            bitmap[byte_index] |= 0x80 >> (x % 8);
        }
    }
    let advance = metrics.advance_width.round().clamp(1.0, 255.0) as u8;
    let left = metrics.xmin.clamp(i32::from(i8::MIN), i32::from(i8::MAX)) as i8;
    let top = metrics.ymin.clamp(i32::from(i8::MIN), i32::from(i8::MAX)) as i8;
    Some(RasterGlyph {
        width: width.max(1),
        height: height.max(1),
        advance: advance.max(px / 4).max(1),
        left,
        top,
        bitmap,
    })
}

#[cfg(test)]
mod tests {
    use super::is_open_font_path;
    use std::path::Path;

    #[test]
    fn accepts_open_ttf_otf_names() {
        assert!(is_open_font_path(Path::new("NOTOSC.TTF")));
        assert!(is_open_font_path(Path::new("source.otf")));
        assert!(!is_open_font_path(Path::new("README.TXT")));
    }
}
