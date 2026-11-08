//! Baked 1-bit bitmap font and a tiny text renderer.
//!
//! The glyph data lives in `font_data.rs`, generated offline by
//! `tools/gen_font.py` from DejaVu Sans Mono. Four faces are baked, all with
//! ASCII + Latin-1 + Latin Extended-A + the UI symbols: `SMALL` (40 px:
//! status lines, key guide, details), `BODY` (48 px: rows), `BODY_BOLD`
//! (48 px bold: the selected row and the header) and `BIG` (72 px: titles,
//! numbers). Sizes are for the 1080 x 2400 panel (the canvas boards are
//! 390 px wide, so their 12-16 px become 33-44 px here; nothing is under 40). Characters that
//! are not in the atlas are drawn as a dim box, never silently folded.

use crate::display::Canvas;

/// One glyph. Bitmap rows are packed MSB-first, padded to a whole byte.
pub struct Glyph {
    /// Unicode code point.
    pub cp: u32,
    /// Horizontal advance in pixels.
    pub adv: u16,
    /// Left bearing of the bitmap relative to the pen position.
    pub x: i16,
    /// Top of the bitmap relative to the top of the line box.
    pub y: i16,
    /// Bitmap width and height in pixels.
    pub w: u16,
    pub h: u16,
    /// Byte offset of the first row in `FontData::bits`.
    pub off: u32,
}

/// A baked font size.
pub struct FontData {
    pub size_px: u16,
    pub ascent: i16,
    /// Height of one line box (ascent + descent).
    #[allow(dead_code)] // part of the generated table; layout uses fixed row heights
    pub line_h: u16,
    /// Sorted by `cp` (binary search).
    pub glyphs: &'static [Glyph],
    pub bits: &'static [u8],
}

pub use crate::font_data::{BIG, BODY, BODY_BOLD, SMALL};

impl FontData {
    /// Looks up the glyph for `ch`.
    pub fn glyph(&self, ch: char) -> Option<&Glyph> {
        let cp = ch as u32;
        self.glyphs.binary_search_by_key(&cp, |g| g.cp).ok().map(|i| &self.glyphs[i])
    }

    /// Advance used for a missing glyph (the box) and as a fallback.
    fn missing_adv(&self) -> u16 {
        self.glyph('M').map(|g| g.adv).unwrap_or(self.size_px / 2)
    }

    /// Width in pixels of `s` when drawn with this font.
    pub fn text_width(&self, s: &str) -> u32 {
        s.chars()
            .map(|c| self.glyph(c).map(|g| g.adv).unwrap_or_else(|| self.missing_adv()) as u32)
            .sum()
    }

    /// Cuts `s` so that it fits into `max_w` pixels, ending with "…" when cut.
    pub fn ellipsize(&self, s: &str, max_w: u32) -> String {
        if self.text_width(s) <= max_w {
            return s.to_string();
        }
        let ell = self.text_width("…");
        let mut out = String::new();
        let mut w = 0;
        for c in s.chars() {
            let cw = self.glyph(c).map(|g| g.adv as u32).unwrap_or(self.missing_adv() as u32);
            if w + cw + ell > max_w {
                break;
            }
            w += cw;
            out.push(c);
        }
        out.push('…');
        out
    }

    /// Draws `s` with the top of the line box at `y`. Returns the advance.
    pub fn draw(&self, cv: &mut Canvas, x: i32, y: i32, s: &str, color: u32) -> u32 {
        let mut pen = x;
        for c in s.chars() {
            match self.glyph(c) {
                Some(g) => {
                    self.draw_glyph(cv, pen, y, g, color);
                    pen += g.adv as i32;
                }
                None => {
                    // Dim outlined box for characters outside the atlas.
                    let adv = self.missing_adv() as i32;
                    let top = y + self.ascent as i32 * 3 / 10;
                    let h = self.ascent as i32 * 7 / 10;
                    cv.outline(pen + 2, top, adv - 4, h, 2, crate::ui::DIM);
                    pen += adv;
                }
            }
        }
        (pen - x) as u32
    }

    fn draw_glyph(&self, cv: &mut Canvas, pen: i32, y: i32, g: &Glyph, color: u32) {
        let stride = (g.w as usize).div_ceil(8);
        let base = g.off as usize;
        for row in 0..g.h as usize {
            let py = y + g.y as i32 + row as i32;
            let line = &self.bits[base + row * stride..base + (row + 1) * stride];
            for col in 0..g.w as usize {
                if line[col / 8] & (0x80 >> (col % 8)) != 0 {
                    cv.put(pen + g.x as i32 + col as i32, py, color);
                }
            }
        }
    }
}
