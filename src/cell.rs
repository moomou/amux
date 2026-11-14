//! Compact cell representation (12 bytes per cell).
//!
//! `fg` and `bg` each hold a 26-bit color (2-bit kind + 24-bit value) and use
//! their top 6 bits for attributes, so a full styled cell fits in three u32s.

use std::collections::HashMap;

/// Empty cell (renders as a space).
pub const BLANK: u32 = 0;
/// Right half of a wide character.
pub const WIDE_SPACER: u32 = 0x11_0001;
/// `ch` values with this bit set index into a [`Clusters`] table.
pub const CLUSTER_BIT: u32 = 0x8000_0000;

pub const COLOR_MASK: u32 = 0x03FF_FFFF;
const KIND_SHIFT: u32 = 24;

// Attribute bits stored in `fg`.
pub const BOLD: u32 = 1 << 26;
pub const DIM: u32 = 1 << 27;
pub const ITALIC: u32 = 1 << 28;
pub const BLINK: u32 = 1 << 29;
pub const REVERSE: u32 = 1 << 30;
pub const HIDDEN: u32 = 1 << 31;
// Attribute bits stored in `bg`.
pub const STRIKE: u32 = 1 << 26;
pub const OVERLINE: u32 = 1 << 27;
pub const UL_SHIFT: u32 = 28;
pub const UL_MASK: u32 = 0b111 << UL_SHIFT;

#[inline]
pub const fn color_default() -> u32 {
    0
}
#[inline]
pub const fn color_idx(i: u8) -> u32 {
    (1 << KIND_SHIFT) | i as u32
}
#[inline]
pub const fn color_rgb(r: u8, g: u8, b: u8) -> u32 {
    (2 << KIND_SHIFT) | (r as u32) << 16 | (g as u32) << 8 | b as u32
}

/// Decoded color.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Color {
    Default,
    Idx(u8),
    Rgb(u8, u8, u8),
}

#[inline]
pub fn color_of(v: u32) -> Color {
    let v = v & COLOR_MASK;
    match v >> KIND_SHIFT {
        1 => Color::Idx(v as u8),
        2 => Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8),
        _ => Color::Default,
    }
}

pub fn color_to_u32(c: Color) -> u32 {
    match c {
        Color::Default => 0,
        Color::Idx(i) => color_idx(i),
        Color::Rgb(r, g, b) => color_rgb(r, g, b),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
#[repr(C)]
pub struct Cell {
    pub ch: u32,
    pub fg: u32,
    pub bg: u32,
}

/// Cells as raw bytes (for memcmp-speed row comparison).
#[inline]
pub fn cell_bytes(cells: &[Cell]) -> &[u8] {
    // SAFETY: Cell is #[repr(C)] with three u32 fields and no padding.
    unsafe { std::slice::from_raw_parts(cells.as_ptr() as *const u8, std::mem::size_of_val(cells)) }
}

/// Any cell refers to a grapheme cluster (needs table-aware handling).
#[inline]
pub fn has_cluster(cells: &[Cell]) -> bool {
    cells.iter().fold(0u32, |acc, c| acc | c.ch) & CLUSTER_BIT != 0
}

impl Cell {
    pub const EMPTY: Cell = Cell { ch: BLANK, fg: 0, bg: 0 };

    #[inline]
    pub fn blank_bg(bg: u32) -> Cell {
        Cell { ch: BLANK, fg: 0, bg: bg & COLOR_MASK }
    }
    #[inline]
    pub fn is_spacer(&self) -> bool {
        self.ch == WIDE_SPACER
    }
    /// Blank with default colors and no attributes (trimmable).
    #[inline]
    pub fn is_default_blank(&self) -> bool {
        (self.ch == BLANK || self.ch == ' ' as u32) && self.fg & !(COLOR_MASK) == 0 && self.bg == 0
    }
    #[inline]
    pub fn style(&self) -> Style {
        Style { fg: self.fg, bg: self.bg }
    }
    #[inline]
    pub fn with_style(ch: u32, s: Style) -> Cell {
        Cell { ch, fg: s.fg, bg: s.bg }
    }
}

/// Foreground/background colors plus attribute bits.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
pub struct Style {
    pub fg: u32,
    pub bg: u32,
}

impl Style {
    pub const DEFAULT: Style = Style { fg: 0, bg: 0 };
    #[inline]
    pub fn fg_color(&self) -> Color {
        color_of(self.fg)
    }
    #[inline]
    pub fn bg_color(&self) -> Color {
        color_of(self.bg)
    }
    #[inline]
    pub fn has(&self, fg_attr: u32) -> bool {
        self.fg & fg_attr != 0
    }
    #[inline]
    pub fn underline(&self) -> u32 {
        (self.bg & UL_MASK) >> UL_SHIFT
    }
    #[inline]
    pub fn set_fg(&mut self, c: u32) {
        self.fg = (self.fg & !COLOR_MASK) | (c & COLOR_MASK);
    }
    #[inline]
    pub fn set_bg(&mut self, c: u32) {
        self.bg = (self.bg & !COLOR_MASK) | (c & COLOR_MASK);
    }
    #[inline]
    pub fn set_underline(&mut self, u: u32) {
        self.bg = (self.bg & !UL_MASK) | ((u.min(7)) << UL_SHIFT);
    }
}

/// Interned grapheme clusters (base char + combining marks) for one grid.
#[derive(Default)]
pub struct Clusters {
    strings: Vec<Box<str>>,
    map: HashMap<Box<str>, u32>,
}

pub const MAX_CLUSTERS: usize = 8192;

impl Clusters {
    pub fn intern(&mut self, s: &str) -> u32 {
        if let Some(&i) = self.map.get(s) {
            return i | CLUSTER_BIT;
        }
        let i = self.strings.len() as u32;
        let b: Box<str> = s.into();
        self.strings.push(b.clone());
        self.map.insert(b, i);
        i | CLUSTER_BIT
    }
    pub fn get(&self, ch: u32) -> Option<&str> {
        self.strings.get((ch & !CLUSTER_BIT) as usize).map(|s| &**s)
    }
    pub fn len(&self) -> usize {
        self.strings.len()
    }
    pub fn is_empty(&self) -> bool {
        self.strings.is_empty()
    }
    pub fn clear(&mut self) {
        self.strings.clear();
        self.map.clear();
    }
    /// Set a specific id (used by mirrors receiving clusters over the wire).
    pub fn set(&mut self, id: u32, s: &str) {
        let i = (id & !CLUSTER_BIT) as usize;
        if i > MAX_CLUSTERS * 4 {
            return;
        }
        if self.strings.len() <= i {
            self.strings.resize(i + 1, "".into());
        }
        self.strings[i] = s.into();
    }
}

/// Append the text of a cell to `out`. Blank and spacer cells: blank -> ' ', spacer -> nothing.
#[inline]
pub fn push_cell_text(out: &mut String, c: &Cell, clusters: &Clusters) {
    match c.ch {
        BLANK => out.push(' '),
        WIDE_SPACER => {}
        ch if ch & CLUSTER_BIT != 0 => out.push_str(clusters.get(ch).unwrap_or(" ")),
        ch => out.push(char::from_u32(ch).unwrap_or(' ')),
    }
}

#[inline]
pub fn char_width(c: char) -> usize {
    if (c as u32) < 0x7f {
        return if (c as u32) >= 0x20 { 1 } else { 0 };
    }
    unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Display width of a string.
pub fn str_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// Width of a cell's content (1 or 2) for non-spacer cells.
#[inline]
pub fn cell_width(c: &Cell, clusters: &Clusters) -> usize {
    match c.ch {
        BLANK | WIDE_SPACER => 1,
        ch if ch & CLUSTER_BIT != 0 => clusters.get(ch).and_then(|s| s.chars().next()).map(|ch| char_width(ch).max(1)).unwrap_or(1),
        ch => char::from_u32(ch).map(|c| char_width(c).max(1)).unwrap_or(1),
    }
}

/// xterm 256-color palette to RGB.
pub fn palette_rgb(i: u8) -> (u8, u8, u8) {
    const BASE: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 0, 0),
        (0, 205, 0),
        (205, 205, 0),
        (0, 0, 238),
        (205, 0, 205),
        (0, 205, 205),
        (229, 229, 229),
        (127, 127, 127),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (92, 92, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    if i < 16 {
        return BASE[i as usize];
    }
    if i < 232 {
        let i = i - 16;
        let f = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
        return (f(i / 36), f((i / 6) % 6), f(i % 6));
    }
    let v = 8 + (i - 232) * 10;
    (v, v, v)
}

/// Nearest xterm-256 palette index for an RGB color.
pub fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    let q = |v: u8| -> u8 {
        if v < 48 {
            0
        } else if v < 115 {
            1
        } else {
            (v - 35) / 40
        }
    };
    let (qr, qg, qb) = (q(r), q(g), q(b));
    let cube = 16 + 36 * qr + 6 * qg + qb;
    let (cr, cg, cb) = palette_rgb(cube);
    let avg = (r as u32 + g as u32 + b as u32) / 3;
    let gi = if avg > 238 { 23 } else { (avg.saturating_sub(3) / 10).min(23) } as u8;
    let gray = 232 + gi;
    let (gr, gg, gb) = palette_rgb(gray);
    let d = |a: (u8, u8, u8), b: (u8, u8, u8)| {
        let dr = a.0 as i32 - b.0 as i32;
        let dg = a.1 as i32 - b.1 as i32;
        let db = a.2 as i32 - b.2 as i32;
        dr * dr + dg * dg + db * db
    };
    if d((r, g, b), (gr, gg, gb)) < d((r, g, b), (cr, cg, cb)) { gray } else { cube }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cell_is_12_bytes() {
        assert_eq!(std::mem::size_of::<Cell>(), 12);
    }
    #[test]
    fn color_roundtrip() {
        assert_eq!(color_of(color_idx(200)), Color::Idx(200));
        assert_eq!(color_of(color_rgb(1, 2, 3)), Color::Rgb(1, 2, 3));
        let mut s = Style::default();
        s.fg |= BOLD;
        s.set_fg(color_rgb(9, 8, 7));
        assert!(s.has(BOLD));
        assert_eq!(s.fg_color(), Color::Rgb(9, 8, 7));
        s.set_underline(3);
        s.set_bg(color_idx(4));
        assert_eq!(s.underline(), 3);
        assert_eq!(s.bg_color(), Color::Idx(4));
    }
    #[test]
    fn rgb_mapping() {
        assert_eq!(rgb_to_256(0, 0, 0), 16);
        assert_eq!(rgb_to_256(255, 255, 255), 231);
        assert_eq!(rgb_to_256(128, 128, 128), 244);
    }
}
