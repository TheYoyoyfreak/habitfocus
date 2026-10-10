//! Cover pictures. Terminals with a graphics protocol (kitty, sixel, iTerm2) get
//! the real image through ratatui-image, like the sundial; everything
//! else gets pixel art: the cover shrunk to one pixel per half cell, reduced to
//! the 6×6×6 colour cube and drawn with `▀`.

use image::imageops::FilterType;
use image::{DynamicImage, Rgba, RgbaImage};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::Widget;

pub enum Cover {
    Loading,
    Ready(DynamicImage),
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverMode {
    /// Graphics protocol when the terminal has one, else pixel art.
    Auto,
    Pixel,
    Off,
}

impl CoverMode {
    pub fn next(self) -> CoverMode {
        match self {
            CoverMode::Auto => CoverMode::Pixel,
            CoverMode::Pixel => CoverMode::Off,
            CoverMode::Off => CoverMode::Auto,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            CoverMode::Auto => "auto",
            CoverMode::Pixel => "pixel art",
            CoverMode::Off => "off",
        }
    }
}

/// Blocking: has habitd download the cover (unless it's cached already), then
/// decodes it.
pub fn load(url: &str) -> Result<DynamicImage, String> {
    use super::Backend;
    let path = habit_books::covers::path(url);
    if !path.is_file() {
        super::Ipc.call(habit_books::api::BooksOp::FetchCover { url: url.to_owned() })?;
    }
    image::open(&path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Whether the terminal takes 24-bit colour; otherwise pixel art uses the 256-colour palette.
pub fn truecolor() -> bool {
    std::env::var("COLORTERM").is_ok_and(|v| v == "truecolor" || v == "24bit")
}

/// Largest size with `w:h` aspect that fits in `max_w × max_h`, at least 1×1.
fn fit(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (max_w.max(1), max_h.max(1));
    }
    if w * max_h > h * max_w {
        (max_w.max(1), (h * max_w / w).max(1))
    } else {
        ((w * max_h / h).max(1), max_h.max(1))
    }
}

/// Downscales to fit `cols × rows` cells (two pixels per cell, stacked) and
/// snaps every channel to one of six levels.
pub fn pixelate(img: &DynamicImage, cols: u16, rows: u16) -> RgbaImage {
    let (w, h) = fit(img.width(), img.height(), u32::from(cols), u32::from(rows) * 2);
    let mut small = img.resize_exact(w, h, FilterType::Triangle).to_rgba8();
    for p in small.pixels_mut() {
        for c in &mut p.0[..3] {
            *c = level(*c) * 51;
        }
    }
    small
}

/// 0..=5
fn level(c: u8) -> u8 {
    ((u16::from(c) + 25) / 51) as u8
}

/// Draws an image from [`pixelate`], horizontally centred in the area.
pub struct PixelArt<'a> {
    pub image: &'a RgbaImage,
    pub truecolor: bool,
}

impl PixelArt<'_> {
    fn color(&self, p: &Rgba<u8>) -> Color {
        let [r, g, b, _] = p.0;
        if self.truecolor {
            Color::Rgb(r, g, b)
        } else {
            Color::Indexed(16 + 36 * level(r) + 6 * level(g) + level(b))
        }
    }
}

impl Widget for PixelArt<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let (w, h) = self.image.dimensions();
        let left = area.x + area.width.saturating_sub(w as u16) / 2;
        for (row, y) in (0..h).step_by(2).enumerate() {
            let cy = area.y + row as u16;
            if cy >= area.bottom() {
                break;
            }
            for x in 0..w {
                let cx = left + x as u16;
                if cx >= area.right() {
                    break;
                }
                let top = self.color(self.image.get_pixel(x, y));
                let bottom = if y + 1 < h { self.color(self.image.get_pixel(x, y + 1)) } else { Color::Reset };
                buf[(cx, cy)].set_char('▀').set_fg(top).set_bg(bottom);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_aspect() {
        assert_eq!(fit(200, 300, 20, 20), (13, 20));
        assert_eq!(fit(300, 200, 20, 20), (20, 13));
        assert_eq!(fit(1, 1000, 20, 20), (1, 20));
    }

    #[test]
    fn pixel_art_uses_two_pixels_per_cell_and_six_levels() {
        let img = DynamicImage::ImageRgba8(RgbaImage::from_pixel(200, 300, Rgba([250, 130, 10, 255])));
        let px = pixelate(&img, 40, 10);
        assert_eq!(px.dimensions(), (13, 20));
        assert_eq!(px.get_pixel(0, 0).0, [255, 153, 0, 255]);

        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 10));
        PixelArt { image: &px, truecolor: false }.render(buf.area, &mut buf);
        // Centred: (40 - 13) / 2 = 13 columns of padding.
        assert_eq!(buf[(12, 0)].symbol(), " ");
        let cell = &buf[(13, 0)];
        assert_eq!(cell.symbol(), "▀");
        assert_eq!(cell.fg, Color::Indexed(16 + 36 * 5 + 6 * 3));
    }
}
