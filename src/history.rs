//! Scrollback history stored as packed, width-independent logical lines.
//!
//! Soft-wrapped rows are joined into one logical line, so resizing never
//! requires reflowing history; rows are re-wrapped lazily when viewed.
//! Lines live in ~32 KiB blocks: per-line overhead is a 4-byte offset.
//!
//! Line encoding: `[flags u8]` then runs of `[fg u32][bg u32][len varint][utf8]`.

use crate::cell::*;
use std::collections::VecDeque;

const BLOCK: usize = 32 * 1024;
/// Longest logical line kept as one (longer wrapped lines are split).
const MAX_LINE_BYTES: usize = 64 * 1024;
pub const LINE_PROMPT: u8 = 1;
/// Allocated-but-unwritten cell at the end of a wrapped row (tmux cellsize >
/// cellused): reads as a space, but disappears when the line is re-wrapped.
pub const PAD: char = '\u{FFFF}';

struct Block {
    data: Vec<u8>,
    starts: Vec<u32>,
}

impl Block {
    fn new() -> Block {
        Block { data: Vec::with_capacity(2048), starts: Vec::with_capacity(64) }
    }
    fn line(&self, i: usize) -> &[u8] {
        let s = self.starts[i] as usize;
        let e = self.starts.get(i + 1).map(|&e| e as usize).unwrap_or(self.data.len());
        &self.data[s..e]
    }
}

pub struct History {
    blocks: VecDeque<Block>,
    lines: usize,
    pub limit: usize,
    /// Last line continues into the next pushed row.
    open: bool,
    /// Monotonic count of lines ever started.
    pub pushed: u64,
    /// Monotonic count of lines dropped from the front (limit or clear).
    pub dropped: u64,
    scratch: Vec<u8>,
}

fn put_var(out: &mut Vec<u8>, mut v: usize) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn get_var(b: &[u8], pos: &mut usize) -> Option<usize> {
    let mut v = 0usize;
    let mut shift = 0;
    loop {
        let byte = *b.get(*pos)?;
        *pos += 1;
        v |= ((byte & 0x7f) as usize) << shift;
        if byte & 0x80 == 0 {
            return Some(v);
        }
        shift += 7;
        if shift > 56 {
            return None;
        }
    }
}

/// Encode cells as runs (appended to `out`). Spacers are skipped.
pub fn encode_runs(out: &mut Vec<u8>, cells: &[Cell], clusters: &Clusters) {
    encode_runs_with(out, cells, clusters, &mut Vec::new());
}

/// [`encode_runs`] with a reusable text buffer (no allocation per line).
fn encode_runs_with(out: &mut Vec<u8>, cells: &[Cell], clusters: &Clusters, text: &mut Vec<u8>) {
    let mut i = 0;
    while i < cells.len() {
        let (fg, bg) = (cells[i].fg, cells[i].bg);
        text.clear();
        let mut j = i;
        while j < cells.len() && cells[j].fg == fg && cells[j].bg == bg {
            match cells[j].ch {
                BLANK => text.push(b' '),
                ch if ch < 0x80 => text.push(ch as u8),
                WIDE_SPACER => {}
                ch if ch & CLUSTER_BIT != 0 => text.extend_from_slice(clusters.get(ch).unwrap_or(" ").as_bytes()),
                ch => {
                    let mut b = [0u8; 4];
                    text.extend_from_slice(char::from_u32(ch).unwrap_or(' ').encode_utf8(&mut b).as_bytes());
                }
            }
            j += 1;
        }
        if !text.is_empty() {
            out.extend_from_slice(&fg.to_le_bytes());
            out.extend_from_slice(&bg.to_le_bytes());
            put_var(out, text.len());
            out.extend_from_slice(text);
        }
        i = j;
    }
}

/// Iterate `(style, text)` runs of an encoded line (after the flags byte).
pub struct Runs<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for Runs<'a> {
    type Item = (Style, &'a str);
    fn next(&mut self) -> Option<Self::Item> {
        if self.pos + 8 > self.b.len() {
            return None;
        }
        let fg = u32::from_le_bytes(self.b[self.pos..self.pos + 4].try_into().ok()?);
        let bg = u32::from_le_bytes(self.b[self.pos + 4..self.pos + 8].try_into().ok()?);
        self.pos += 8;
        let len = get_var(self.b, &mut self.pos)?;
        let end = self.pos.checked_add(len)?;
        let s = std::str::from_utf8(self.b.get(self.pos..end)?).ok()?;
        self.pos = end;
        Some((Style { fg, bg }, s))
    }
}

pub fn runs(line: &[u8]) -> Runs<'_> {
    Runs { b: line, pos: 1.min(line.len()) }
}

pub fn line_flags(line: &[u8]) -> u8 {
    line.first().copied().unwrap_or(0)
}

/// Plain text of an encoded line.
pub fn line_text(line: &[u8]) -> String {
    let mut s = String::new();
    for (_, t) in runs(line) {
        if t.contains(PAD) {
            s.extend(t.chars().map(|c| if c == PAD { ' ' } else { c }));
        } else {
            s.push_str(t);
        }
    }
    s
}

/// Convert runs into cells (wide chars get spacers, zero-width chars join clusters).
pub fn cellize<'a>(it: impl Iterator<Item = (Style, &'a str)>, clusters: &mut Clusters) -> Vec<Cell> {
    let mut cells: Vec<Cell> = Vec::new();
    let mut tmp = String::new();
    for (st, text) in it {
        for c in text.chars() {
            if c == PAD {
                continue;
            }
            let w = char_width(c);
            if w == 0 {
                if (c as u32) < 0x20 {
                    continue;
                }
                let mut k = cells.len();
                while k > 0 && cells[k - 1].ch == WIDE_SPACER {
                    k -= 1;
                }
                if k == 0 {
                    continue;
                }
                let prev = cells[k - 1];
                tmp.clear();
                push_cell_text(&mut tmp, &prev, clusters);
                tmp.push(c);
                cells[k - 1].ch = clusters.intern(&tmp);
                continue;
            }
            cells.push(Cell::with_style(c as u32, st));
            if w == 2 {
                cells.push(Cell::with_style(WIDE_SPACER, st));
            }
        }
    }
    cells
}

/// Split a logical line into rows of `width` (wide chars never straddle rows).
/// Returns rows; every row except the last is marked wrapped.
pub fn wrap_cells(cells: &[Cell], width: usize) -> Vec<(Vec<Cell>, bool)> {
    let width = width.max(2);
    let mut rows = Vec::new();
    let mut cur: Vec<Cell> = Vec::with_capacity(width);
    let mut i = 0;
    while i < cells.len() {
        let c = cells[i];
        let wide = cells.get(i + 1).is_some_and(|n| n.ch == WIDE_SPACER) && c.ch != WIDE_SPACER;
        let w = if wide { 2 } else { 1 };
        if c.ch == WIDE_SPACER {
            i += 1;
            continue;
        }
        if cur.len() + w > width {
            // Rows are not padded: a wide char that does not fit leaves the
            // last column unwritten (callers pad to the screen width).
            rows.push((std::mem::replace(&mut cur, Vec::with_capacity(width)), true));
        }
        cur.push(c);
        if wide {
            cur.push(cells[i + 1]);
            i += 1;
        }
        i += 1;
    }
    rows.push((cur, false));
    rows
}

impl History {
    pub fn new(limit: usize) -> History {
        History { blocks: VecDeque::new(), lines: 0, limit, open: false, pushed: 0, dropped: 0, scratch: Vec::new() }
    }

    pub fn len(&self) -> usize {
        self.lines
    }
    pub fn is_empty(&self) -> bool {
        self.lines == 0
    }
    pub fn is_open(&self) -> bool {
        self.open
    }
    pub fn close(&mut self) {
        self.open = false;
    }
    pub fn close_if_empty(&mut self) {
        if self.lines == 0 {
            self.open = false;
        }
    }

    pub fn clear(&mut self) {
        self.dropped += self.lines as u64;
        self.blocks.clear();
        self.lines = 0;
        self.open = false;
    }

    /// Approximate heap bytes used.
    pub fn bytes(&self) -> usize {
        self.blocks.iter().map(|b| b.data.capacity() + b.starts.capacity() * 4).sum()
    }

    fn start_line(&mut self, flags: u8) -> &mut Block {
        // Blocks are capped by bytes and by lines, so trimming to the limit
        // only ever drops whole blocks (tiny lines would otherwise pack more
        // than `limit` lines into one block).
        let max_lines = (self.limit / 16).max(16);
        let need_new = match self.blocks.back() {
            None => true,
            Some(b) => b.data.len() >= BLOCK || b.starts.len() >= max_lines,
        };
        if need_new {
            if let Some(b) = self.blocks.back_mut() {
                b.data.shrink_to_fit();
                b.starts.shrink_to_fit();
            }
            self.blocks.push_back(Block::new());
        }
        let b = self.blocks.back_mut().unwrap();
        b.starts.push(b.data.len() as u32);
        b.data.push(flags);
        self.lines += 1;
        self.pushed += 1;
        b
    }

    /// Push a screen row that scrolled off the top.
    pub fn push_row(&mut self, cells: &[Cell], wrapped: bool, mark: u8, clusters: &Clusters) {
        self.push_row_pad(cells, 0, wrapped, mark, clusters);
    }

    /// Push a row followed by `pad` virtual padding cells (see [`PAD`]).
    pub fn push_row_pad(&mut self, cells: &[Cell], pad: usize, wrapped: bool, mark: u8, clusters: &Clusters) {
        if self.limit == 0 {
            return;
        }
        let mut end = cells.len();
        if !wrapped {
            while end > 0 && cells[end - 1].is_default_blank() {
                end -= 1;
            }
        }
        let cells = &cells[..end];
        let mut text = std::mem::take(&mut self.scratch);
        // A line that never ends (no newlines) is split so memory stays
        // bounded by the line limit.
        let continuing = self.open && self.blocks.back().is_some_and(|b| b.data.len() - *b.starts.last().unwrap_or(&0) as usize <= MAX_LINE_BYTES);
        let b = if continuing { self.blocks.back_mut().unwrap() } else { self.start_line(mark) };
        encode_runs_with(&mut b.data, cells, clusters, &mut text);
        if pad > 0 {
            b.data.extend_from_slice(&0u32.to_le_bytes());
            b.data.extend_from_slice(&0u32.to_le_bytes());
            put_var(&mut b.data, pad * PAD.len_utf8());
            for _ in 0..pad {
                let mut buf = [0u8; 4];
                b.data.extend_from_slice(PAD.encode_utf8(&mut buf).as_bytes());
            }
        }
        self.scratch = text;
        self.open = wrapped;
        self.trim();
    }

    /// Push a pre-encoded line (flags + runs).
    pub fn push_encoded(&mut self, line: &[u8]) {
        let flags = line_flags(line);
        let b = self.start_line(flags);
        if line.len() > 1 {
            b.data.extend_from_slice(&line[1..]);
        }
        self.open = false;
        self.trim();
    }

    pub fn trim(&mut self) {
        while self.lines > self.limit {
            let n = match self.blocks.front() {
                Some(b) => b.starts.len(),
                None => break,
            };
            // Only drop whole blocks; keep within one block of the limit.
            if self.lines - n < self.limit && self.blocks.len() > 1 {
                break;
            }
            if self.blocks.len() == 1 {
                // Single oversized block: rebuild without the oldest lines.
                let drop = self.lines - self.limit;
                let b = self.blocks.pop_front().unwrap();
                if drop >= b.starts.len() {
                    self.dropped += self.lines as u64;
                    self.lines = 0;
                    self.open = false;
                    break;
                }
                let mut nb = Block::new();
                for i in drop..b.starts.len() {
                    nb.starts.push(nb.data.len() as u32);
                    nb.data.extend_from_slice(b.line(i));
                }
                self.lines -= drop;
                self.dropped += drop as u64;
                self.blocks.push_back(nb);
                break;
            }
            self.blocks.pop_front();
            self.lines -= n;
            self.dropped += n as u64;
        }
    }

    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit;
        self.trim();
    }

    /// Encoded line `i` (0 = oldest).
    pub fn line(&self, mut i: usize) -> Option<&[u8]> {
        if i >= self.lines {
            return None;
        }
        for b in &self.blocks {
            if i < b.starts.len() {
                return Some(b.line(i));
            }
            i -= b.starts.len();
        }
        None
    }

    /// Iterate encoded lines oldest-first.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &[u8]> + '_ {
        self.blocks.iter().flat_map(|b| (0..b.starts.len()).map(move |i| b.line(i)))
    }

    /// Remove and return the newest line (used when the screen grows taller).
    pub fn pop_newest(&mut self) -> Option<Vec<u8>> {
        let b = self.blocks.back_mut()?;
        let s = b.starts.pop()? as usize;
        let line = b.data[s..].to_vec();
        b.data.truncate(s);
        if b.starts.is_empty() {
            self.blocks.pop_back();
        }
        self.lines -= 1;
        self.open = false;
        Some(line)
    }

    /// Serialize (for crash/reboot snapshots).
    pub fn serialize(&self, out: &mut Vec<u8>) {
        put_var(out, self.lines);
        for l in self.iter() {
            put_var(out, l.len());
            out.extend_from_slice(l);
        }
    }

    pub fn deserialize(&mut self, b: &[u8], pos: &mut usize) -> Option<()> {
        let n = get_var(b, pos)?;
        for _ in 0..n {
            let len = get_var(b, pos)?;
            let end = pos.checked_add(len)?;
            let l = b.get(*pos..end)?;
            *pos = end;
            if !l.is_empty() {
                self.push_encoded(l);
            }
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(s: &str) -> Vec<Cell> {
        s.chars().map(|c| Cell { ch: c as u32, fg: 0, bg: 0 }).collect()
    }

    #[test]
    fn push_and_join_wrapped() {
        let cl = Clusters::default();
        let mut h = History::new(100);
        h.push_row(&row("hello     "), false, 0, &cl);
        h.push_row(&row("abcdefghij"), true, 0, &cl);
        h.push_row(&row("klm       "), false, 0, &cl);
        assert_eq!(h.len(), 2);
        assert_eq!(line_text(h.line(0).unwrap()), "hello");
        assert_eq!(line_text(h.line(1).unwrap()), "abcdefghijklm");
    }

    #[test]
    fn limit_trims_blocks() {
        let cl = Clusters::default();
        let mut h = History::new(1000);
        let r = row(&"x".repeat(80));
        for _ in 0..20000 {
            h.push_row(&r, false, 0, &cl);
        }
        assert!(h.len() >= 1000 && h.len() < 1000 + 600, "{}", h.len());
        // ~90 bytes per line, far below a cell grid (80*12)
        assert!(h.bytes() < h.len() * 140, "{} bytes for {} lines", h.bytes(), h.len());
    }

    #[test]
    fn wrap_wide() {
        let mut cl = Clusters::default();
        let cells = cellize([(Style::DEFAULT, "ab漢字c")].into_iter(), &mut cl);
        assert_eq!(cells.len(), 7);
        let rows = wrap_cells(&cells, 3);
        // "ab" (漢 can't straddle), then "漢" + "字" wraps again
        assert_eq!(rows[0].0.len(), 2);
        assert!(rows[0].1);
        assert!(!rows.last().unwrap().1);
    }

    #[test]
    fn combining_cluster() {
        let mut cl = Clusters::default();
        let cells = cellize([(Style::DEFAULT, "e\u{301}x")].into_iter(), &mut cl);
        assert_eq!(cells.len(), 2);
        assert_eq!(cl.get(cells[0].ch), Some("e\u{301}"));
    }

    #[test]
    fn serialize_roundtrip() {
        let cl = Clusters::default();
        let mut h = History::new(100);
        h.push_row(&row("one"), false, LINE_PROMPT, &cl);
        h.push_row(&row("two"), false, 0, &cl);
        let mut buf = Vec::new();
        h.serialize(&mut buf);
        let mut h2 = History::new(100);
        let mut pos = 0;
        h2.deserialize(&buf, &mut pos).unwrap();
        assert_eq!(h2.len(), 2);
        assert_eq!(line_flags(h2.line(0).unwrap()), LINE_PROMPT);
        assert_eq!(line_text(h2.line(1).unwrap()), "two");
    }
}
