//! Drawing module screens (modules.rs `ScreenView`) in the recovery look of ui.rs: a
//! letter-spaced accent title, an optional dim subline, text lines or rows with "▸" + accent on
//! the selected one, a dim value right, and the key-guide footer. Same measures as ui.rs.
//! A list screen's `header` (menu:header) is drawn between the title and the rows, each line in
//! its `style`: big (the song), plain, dim (details), head (a small spaced accent heading).

use crate::display::Canvas;
use crate::font::{FontData, BIG, BODY, BODY_BOLD, SMALL};
use crate::modules::{Body, Line, ScreenView, Style};
use crate::ui::{ACCENT, BG, DIM, LINE, TEXT, WARN};

const MARGIN: i32 = 64;
const TOP: i32 = 120;
const ROW_H: i32 = 84;
const SMALL_LH: i32 = 54;
const FOOTER_BOTTOM: i32 = 2400 - 64;

fn wrap(f: &FontData, text: &str, max_w: u32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let cand = if cur.is_empty() { word.to_string() } else { format!("{cur} {word}") };
        if f.text_width(&cand) <= max_w || cur.is_empty() {
            cur = cand;
        } else {
            lines.push(std::mem::take(&mut cur));
            cur = word.to_string();
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines.into_iter().map(|l| f.ellipsize(&l, max_w)).collect()
}

/// At most `n` wrapped lines; what does not fit ends the last one with "…".
fn wrap_n(f: &FontData, text: &str, max_w: u32, n: usize) -> Vec<String> {
    let mut l = wrap(f, text, max_w);
    if l.len() > n {
        let rest = l.split_off(n - 1).join(" ");
        l.push(f.ellipsize(&rest, max_w));
    }
    l
}

fn footer(cv: &mut Canvas, lines: &[(String, String)]) -> i32 {
    let top = FOOTER_BOTTOM - lines.len() as i32 * SMALL_LH;
    let mut y = top;
    cv.fill_rect(MARGIN, y - 18, cv.w as i32 - 2 * MARGIN, 2, LINE);
    let col = lines.iter().map(|(k, _)| SMALL.text_width(k)).max().unwrap_or(0) as i32 + SMALL.text_width("  ") as i32;
    for (k, m) in lines {
        SMALL.draw(cv, MARGIN, y, k, DIM);
        SMALL.draw(cv, MARGIN + col, y, m, TEXT);
        y += SMALL_LH;
    }
    top
}

fn title(cv: &mut Canvas, t: &str, sub: Option<&str>) -> i32 {
    let max_w = (cv.w as i32 - 2 * MARGIN) as u32;
    let t = BODY_BOLD.ellipsize(t, max_w.saturating_sub(4 * t.chars().count() as u32).max(200));
    let mut x = MARGIN;
    for c in t.chars() {
        x += BODY_BOLD.draw(cv, x, TOP, &c.to_string(), ACCENT) as i32 + 4;
    }
    let mut y = TOP + 76;
    if let Some(s) = sub {
        for l in wrap(&SMALL, s, max_w).into_iter().take(2) {
            SMALL.draw(cv, MARGIN, y, &l, DIM);
            y += SMALL_LH;
        }
    }
    y + 36
}

/// The key guide: the module's own (`guide`), else `default`, always ending in the
/// "back to Android" line.
fn guide_lines(v: &ScreenView, default: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut g: Vec<(String, String)> = if v.guide.is_empty() {
        default.iter().map(|(k, m)| (k.to_string(), m.to_string())).collect()
    } else {
        v.guide.clone()
    };
    if !g.iter().any(|(k, _)| k.contains("hold ●")) {
        g.push(("hold ● 3 s".into(), "back to Android".into()));
    }
    g
}

/// Letter-spaced text (titles and headings), returns the width drawn.
fn spaced(cv: &mut Canvas, f: &FontData, x0: i32, y: i32, t: &str, c: u32, gap: i32) -> i32 {
    let mut x = x0;
    for ch in t.chars() {
        x += f.draw(cv, x, y, &ch.to_string(), c) as i32 + gap;
    }
    x - x0
}

/// Header lines from `y`, never below `limit`; returns the y after them.
fn header_lines(cv: &mut Canvas, lines: &[Line], mut y: i32, limit: i32, max_w: u32) -> i32 {
    for (i, l) in lines.iter().enumerate() {
        let (font, c, lh, max_lines) = match l.style {
            Style::Big => (&BIG, TEXT, 96, 2),
            Style::Plain => (&BODY, TEXT, 64, 1),
            Style::Dim => (&BODY, DIM, 64, 2),
            Style::Head => (&SMALL, ACCENT, SMALL_LH + 6, 1),
        };
        if l.style == Style::Head && i > 0 {
            y += 28;
        }
        if y + lh > limit {
            break;
        }
        if l.style == Style::Head {
            spaced(cv, font, MARGIN, y, &font.ellipsize(&l.text, max_w), c, 3);
            y += lh;
            continue;
        }
        for wl in wrap_n(font, &l.text, max_w, max_lines) {
            if y + lh > limit {
                break;
            }
            font.draw(cv, MARGIN, y, &wl, c);
            y += lh;
        }
    }
    y
}

const LIST_GUIDE: &[(&str, &str)] = &[("▲ ▼", "move"), ("●", "select"), ("▲+▼", "back")];

/// A module screen. `msg`: the daemon's feedback line, if any.
pub fn draw(cv: &mut Canvas, v: &ScreenView, msg: Option<(&str, u32)>) {
    cv.clear(BG);
    let max_w = (cv.w as i32 - 2 * MARGIN) as u32;
    let mut y = title(cv, &v.title, v.sub.as_deref());
    let foot = footer(cv, &guide_lines(v, LIST_GUIDE));
    if !v.header.is_empty() {
        // Leave room for three rows under the header.
        y = header_lines(cv, &v.header, y - 12, foot - 30 - SMALL_LH - 3 * ROW_H, max_w) + 24;
    }
    if let Some((n, warn)) = &v.note {
        for l in wrap(&BODY, n, max_w).into_iter().take(3) {
            BODY.draw(cv, MARGIN, y, &l, if *warn { WARN } else { DIM });
            y += 64;
        }
        y += 20;
    }
    let rows = match &v.body {
        Body::Rows(r) => r,
        Body::Text(lines, r) => {
            for l in lines {
                for w in wrap(&BODY, l, max_w).into_iter().take(2) {
                    if y + 64 > foot - 2 * ROW_H {
                        break;
                    }
                    BODY.draw(cv, MARGIN, y, &w, TEXT);
                    y += 64;
                }
            }
            y += 30;
            r
        }
    };
    let vis = (((foot - 30 - SMALL_LH) - y) / ROW_H).max(1) as usize;
    let top = if v.sel >= vis { v.sel + 1 - vis } else { 0 };
    let w = cv.w as i32;
    let marker_w = BODY.text_width("▸ ") as i32;
    for (i, row) in rows.iter().enumerate().skip(top).take(vis) {
        let ry = y + (i - top) as i32 * ROW_H;
        let on = i == v.sel;
        let (font, lc, mc) = if on {
            (&BODY_BOLD, ACCENT, ACCENT)
        } else {
            (&BODY, if row.dim { DIM } else { TEXT }, DIM)
        };
        let meta_w = if row.meta.is_empty() {
            0
        } else {
            let m = SMALL.ellipsize(&row.meta, max_w * 2 / 5);
            let mw = SMALL.text_width(&m) as i32;
            SMALL.draw(cv, w - MARGIN - mw, ry + 8, &m, mc);
            mw + 24
        };
        let room = (max_w as i32 - marker_w - meta_w).max(80) as u32;
        if on {
            BODY_BOLD.draw(cv, MARGIN, ry, "▸", ACCENT);
        }
        font.draw(cv, MARGIN + marker_w, ry, &font.ellipsize(&row.label, room), lc);
    }
    if top > 0 {
        SMALL.draw(cv, w - MARGIN - 30, y - 50, "▲", DIM);
    }
    if top + vis < rows.len() {
        SMALL.draw(cv, w - MARGIN - 30, y + vis as i32 * ROW_H - 10, "▼", DIM);
    }
    if let Some((m, c)) = msg {
        SMALL.draw(cv, MARGIN, foot - 30 - SMALL_LH, &SMALL.ellipsize(m, max_w), c);
    }
}

/// Now Playing for a module stream: the state word and volume on top, the `show` screen's
/// title and lines (first two big: artist, title), then the guide.
pub fn draw_now_playing(
    cv: &mut Canvas,
    v: &ScreenView,
    state: (&str, u32),
    volume: (u32, u32),
    default_guide: &[(&str, &str)],
    msg: Option<(&str, u32)>,
) {
    cv.clear(BG);
    let w = cv.w as i32;
    let max_w = (w - 2 * MARGIN) as u32;
    let mut y = TOP;
    SMALL.draw(cv, MARGIN, y, state.0, state.1);
    let vol = format!("vol {} / {}", volume.0, volume.1);
    SMALL.draw(cv, w - MARGIN - SMALL.text_width(&vol) as i32, y, &vol, DIM);
    y += SMALL_LH + 20;
    for l in wrap(&BODY_BOLD, &v.title, max_w).into_iter().take(1) {
        BODY_BOLD.draw(cv, MARGIN, y, &l, ACCENT);
        y += 80;
    }
    let foot = footer(cv, &guide_lines(v, default_guide));
    if !v.header.is_empty() {
        y = header_lines(cv, &v.header, y, foot - 2 * SMALL_LH, max_w);
    } else if let Body::Text(lines, _) = &v.body {
        for (i, l) in lines.iter().enumerate() {
            let (font, c, lh, max_lines) = match i {
                0 => (&BIG, TEXT, 96, 2),
                1 => (&BIG, TEXT, 96, 2),
                _ => (&BODY, DIM, 64, 2),
            };
            for wl in wrap(font, l, max_w).into_iter().take(max_lines) {
                if y + lh > foot - 2 * SMALL_LH {
                    break;
                }
                font.draw(cv, MARGIN, y, &wl, c);
                y += lh;
            }
            if i == 1 {
                y += 20;
            }
        }
    }
    let bar = y + 10;
    cv.fill_rect(MARGIN, bar, w - 2 * MARGIN, 3, LINE);
    let f = (volume.0.min(volume.1) as f64 / volume.1.max(1) as f64 * (w - 2 * MARGIN) as f64) as i32;
    cv.fill_rect(MARGIN, bar, f, 3, ACCENT);
    if let Some((m, c)) = msg {
        SMALL.draw(cv, MARGIN, foot - 30 - SMALL_LH, &SMALL.ellipsize(m, max_w), c);
    } else if let Some((n, warn)) = &v.note {
        SMALL.draw(cv, MARGIN, foot - 30 - SMALL_LH, &SMALL.ellipsize(n, max_w), if *warn { WARN } else { DIM });
    }
}
