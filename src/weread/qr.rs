//! Login QR modules for the e-paper panel.

use qrcode::{Color, QrCode};

use crate::weread::limits::MAX_QR_CHARS;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QrGrid {
    pub size: usize,
    pub dark: Vec<bool>,
}

pub fn qr_grid(text: &str) -> Result<QrGrid, &'static str> {
    if text.is_empty() || text.len() > MAX_QR_CHARS {
        return Err("QR text is empty or too long");
    }
    let code = QrCode::new(text.as_bytes()).map_err(|_| "QR payload does not fit")?;
    let size = code.width();
    if size == 0 || size > 177 {
        return Err("QR size is out of range");
    }
    let mut dark = Vec::with_capacity(size * size);
    for y in 0..size {
        for x in 0..size {
            dark.push(code[(x, y)] == Color::Dark);
        }
    }
    Ok(QrGrid { size, dark })
}

impl QrGrid {
    #[must_use]
    pub fn dark_at(&self, x: usize, y: usize) -> bool {
        self.dark.get(y * self.size + x).copied().unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::qr_grid;

    #[test]
    fn login_url_renders_a_square_code() {
        let grid = qr_grid("https://weread.qq.com/web/confirm?uid=abc123").unwrap();
        assert!(grid.size >= 21);
        assert_eq!(grid.dark.len(), grid.size * grid.size);
        assert!(grid.dark.iter().any(|module| *module));
        assert!(qr_grid("").is_err());
        assert!(qr_grid(&"x".repeat(200)).is_err());
    }
}
