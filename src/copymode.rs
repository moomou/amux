//! Copy mode (lives in the keeper, next to the scrollback it browses).
//!
//! The view addresses a virtual buffer: history rows (logical lines wrapped at
//! the pane width, oldest first) followed by the live screen rows. Indexes are
//! stable while new output arrives, so the view does not jump.

use crate::cell::*;
use crate::history;
use crate::term::{Row, Term};
use std::collections::{HashMap, VecDeque};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pos {
    pub row: usize,
    pub col: usize,
}

pub struct CopyMode {
    pub active: bool,
    width: usize,
    /// Row count of each history logical line at `width`.
    line_rows: VecDeque<u32>,
    hist_rows: usize,
    pushed_seen: u64,
    dropped_seen: u64,
    open_seen: bool,
    pub top: usize,
    pub cx: usize,
    pub cy: usize,
    sel: Option<Pos>,
    rect: bool,
    line_sel: bool,
    pub search: Option<(String, bool)>,
    jump: Option<(char, char)>,
    /// Exit when scrolled back to the bottom (copy-mode -e).
    pub scroll_exit: bool,
    cache: HashMap<usize, Vec<(Vec<Cell>, bool)>>,
    pub clusters: Clusters,
    pub dirty: bool,
    pub message: Option<String>,
    word_seps: String,
}

pub enum Outcome {
    None,
    Copy(String, bool, Option<String>),
    Exit,
}

impl Default for CopyMode {
    fn default() -> Self {
        Self::new()
    }
}

impl CopyMode {
    pub fn new() -> CopyMode {
        CopyMode {
            active: false,
            width: 0,
            line_rows: VecDeque::new(),
            hist_rows: 0,
            pushed_seen: 0,
            dropped_seen: 0,
            open_seen: false,
            top: 0,
            cx: 0,
            cy: 0,
            sel: None,
            rect: false,
            line_sel: false,
            search: None,
            jump: None,
            scroll_exit: false,
            cache: HashMap::new(),
            clusters: Clusters::default(),
            dirty: false,
            message: None,
            word_seps: " -_@".into(),
        }
    }

    fn line_row_count(line: &[u8], width: usize, clusters: &mut Clusters) -> u32 {
        let cells = history::cellize(history::runs(line), clusters);
        history::wrap_cells(&cells, width).len() as u32
    }

    fn rebuild(&mut self, t: &Term) {
        self.width = t.cols();
        self.line_rows.clear();
        self.cache.clear();
        self.clusters.clear();
        let mut cl = Clusters::default();
        for l in t.hist.iter() {
            let n = Self::line_row_count(l, self.width, &mut cl);
            self.line_rows.push_back(n);
        }
        self.hist_rows = self.line_rows.iter().map(|&n| n as usize).sum();
        self.pushed_seen = t.hist.pushed;
        self.dropped_seen = t.hist.dropped;
        self.open_seen = t.hist.is_open();
    }

    /// Bring the index up to date with new history; keeps the view anchored.
    pub fn sync(&mut self, t: &Term) {
        if !self.active {
            return;
        }
        if t.cols() != self.width
            || t.hist.len() != self.line_rows.len() + (t.hist.pushed - self.pushed_seen) as usize - (t.hist.dropped - self.dropped_seen) as usize
        {
            let old_total = self.total_rows(t);
            let from_bottom = old_total.saturating_sub(self.top + t.rows());
            self.rebuild(t);
            let total = self.total_rows(t);
            self.top = total.saturating_sub(t.rows() + from_bottom);
            self.sel = None;
            self.dirty = true;
            return;
        }
        let mut changed = false;
        let dropped = (t.hist.dropped - self.dropped_seen) as usize;
        if dropped > 0 {
            let mut rows = 0usize;
            for _ in 0..dropped {
                rows += self.line_rows.pop_front().unwrap_or(0) as usize;
            }
            self.hist_rows -= rows.min(self.hist_rows);
            self.top = self.top.saturating_sub(rows);
            if let Some(s) = self.sel.as_mut() {
                s.row = s.row.saturating_sub(rows);
            }
            self.dropped_seen = t.hist.dropped;
            changed = true;
        }
        let new = (t.hist.pushed - self.pushed_seen) as usize;
        if new > 0 || self.open_seen {
            // The last known line may have grown if it was open.
            let mut cl = Clusters::default();
            let start = t.hist.len() - new - usize::from(self.open_seen && !self.line_rows.is_empty());
            if self.open_seen
                && let Some(n) = self.line_rows.pop_back()
            {
                self.hist_rows -= n as usize;
            }
            for i in start..t.hist.len() {
                if let Some(l) = t.hist.line(i) {
                    let n = Self::line_row_count(l, self.width, &mut cl);
                    self.line_rows.push_back(n);
                    self.hist_rows += n as usize;
                }
            }
            self.pushed_seen = t.hist.pushed;
            self.open_seen = t.hist.is_open();
            self.cache.retain(|&k, _| k < start);
            changed = true;
        }
        if changed {
            self.dirty = true;
        }
    }

    pub fn total_rows(&self, t: &Term) -> usize {
        self.hist_rows + t.rows()
    }

    pub fn enter(&mut self, t: &Term, scroll_exit: bool) {
        if !self.active {
            self.active = true;
            self.rebuild(t);
            self.top = self.hist_rows;
            let (x, y) = t.cursor();
            self.cx = x;
            self.cy = y;
            self.sel = None;
            self.rect = false;
            self.line_sel = false;
            self.message = None;
        }
        self.scroll_exit = scroll_exit;
        self.dirty = true;
    }

    pub fn exit(&mut self) {
        self.active = false;
        self.cache.clear();
        self.line_rows.clear();
        self.clusters.clear();
        self.sel = None;
        self.dirty = true;
    }

    /// Row `r` of the virtual buffer: (cells, wrapped).
    pub fn row(&mut self, t: &Term, r: usize) -> (Vec<Cell>, bool) {
        if r >= self.hist_rows {
            let y = r - self.hist_rows;
            if y < t.rows() {
                let row: &Row = t.row(y);
                let mut cells = row.cells.clone();
                // Re-intern clusters into our table.
                for c in cells.iter_mut() {
                    if c.ch & CLUSTER_BIT != 0 {
                        c.ch = self.clusters.intern(t.clusters.get(c.ch).unwrap_or(" "));
                    }
                }
                return (cells, row.wrapped);
            }
            return (vec![Cell::EMPTY; t.cols()], false);
        }
        // Locate the logical line: walk from whichever end is closer.
        let (mut idx, mut base);
        if r * 2 > self.hist_rows {
            idx = self.line_rows.len();
            base = self.hist_rows;
            while idx > 0 {
                idx -= 1;
                base -= self.line_rows[idx] as usize;
                if base <= r {
                    break;
                }
            }
        } else {
            idx = 0;
            base = 0;
            while idx < self.line_rows.len() && base + self.line_rows[idx] as usize <= r {
                base += self.line_rows[idx] as usize;
                idx += 1;
            }
        }
        let width = self.width;
        if !self.cache.contains_key(&idx) {
            if self.cache.len() > 256 {
                self.cache.clear();
            }
            let rows = match t.hist.line(idx) {
                Some(l) => {
                    let cells = history::cellize(history::runs(l), &mut self.clusters);
                    history::wrap_cells(&cells, width)
                }
                None => vec![(Vec::new(), false)],
            };
            self.cache.insert(idx, rows);
        }
        let rows = &self.cache[&idx];
        let sub = r - base;
        let (mut cells, mut wrapped) = rows.get(sub).cloned().unwrap_or((Vec::new(), false));
        if sub + 1 == rows.len() && idx + 1 == self.line_rows.len() && t.hist.is_open() {
            wrapped = true;
        }
        cells.resize(width, Cell::EMPTY);
        if cells.len() > width {
            cells.truncate(width);
            wrapped = false;
        }
        (cells, wrapped)
    }

    fn row_text(&mut self, t: &Term, r: usize) -> (String, bool, Vec<usize>) {
        // Returns text, wrapped, and byte offset -> column map.
        let (cells, wrapped) = self.row(t, r);
        let mut s = String::new();
        let mut cols = Vec::new();
        for (x, c) in cells.iter().enumerate() {
            if c.ch == WIDE_SPACER {
                continue;
            }
            let start = s.len();
            push_cell_text(&mut s, c, &self.clusters);
            for _ in start..s.len() {
                cols.push(x);
            }
        }
        (s, wrapped, cols)
    }

    fn abs_cursor(&self) -> Pos {
        Pos { row: self.top + self.cy, col: self.cx }
    }

    fn set_abs(&mut self, t: &Term, p: Pos) {
        let rows = t.rows();
        let total = self.total_rows(t);
        let row = p.row.min(total.saturating_sub(1));
        if row < self.top {
            self.top = row;
        } else if row >= self.top + rows {
            self.top = row + 1 - rows;
        }
        self.top = self.top.min(total.saturating_sub(rows));
        self.cy = row - self.top;
        self.cx = p.col.min(t.cols() - 1);
    }

    fn line_len(&mut self, t: &Term, r: usize) -> usize {
        let (cells, _) = self.row(t, r);
        let mut end = cells.len();
        while end > 0 && cells[end - 1].is_default_blank() {
            end -= 1;
        }
        end
    }

    fn is_sep(&self, c: char) -> bool {
        c == ' ' || self.word_seps.contains(c)
    }

    fn char_at(&mut self, t: &Term, p: Pos) -> char {
        let (cells, _) = self.row(t, p.row);
        match cells.get(p.col) {
            Some(c) if c.ch == BLANK || c.ch == WIDE_SPACER => ' ',
            Some(c) if c.ch & CLUSTER_BIT != 0 => self.clusters.get(c.ch).and_then(|s| s.chars().next()).unwrap_or(' '),
            Some(c) => char::from_u32(c.ch).unwrap_or(' '),
            None => ' ',
        }
    }

    fn next_pos(&mut self, t: &Term, p: Pos) -> Option<Pos> {
        let cols = t.cols();
        if p.col + 1 < cols {
            Some(Pos { row: p.row, col: p.col + 1 })
        } else if p.row + 1 < self.total_rows(t) {
            Some(Pos { row: p.row + 1, col: 0 })
        } else {
            None
        }
    }

    fn prev_pos(&mut self, t: &Term, p: Pos) -> Option<Pos> {
        if p.col > 0 {
            Some(Pos { row: p.row, col: p.col - 1 })
        } else if p.row > 0 {
            Some(Pos { row: p.row - 1, col: t.cols() - 1 })
        } else {
            None
        }
    }

    fn cls(&mut self, t: &Term, p: Pos, space_only: bool) -> u8 {
        let c = self.char_at(t, p);
        if c == ' ' {
            0
        } else if !space_only && self.is_sep(c) {
            1
        } else {
            2
        }
    }

    fn next_word(&mut self, t: &Term, space_only: bool) {
        let mut p = self.abs_cursor();
        let start = self.cls(t, p, space_only);
        // skip current class
        while let Some(n) = self.next_pos(t, p) {
            p = n;
            if self.cls(t, p, space_only) != start {
                break;
            }
        }
        // skip spaces
        while self.char_at(t, p) == ' ' {
            match self.next_pos(t, p) {
                Some(n) => p = n,
                None => break,
            }
        }
        self.set_abs(t, p);
    }

    fn next_word_end(&mut self, t: &Term, space_only: bool) {
        let mut p = self.abs_cursor();
        if let Some(n) = self.next_pos(t, p) {
            p = n;
        }
        while self.char_at(t, p) == ' ' {
            match self.next_pos(t, p) {
                Some(n) => p = n,
                None => break,
            }
        }
        let cls = self.cls(t, p, space_only);
        while let Some(n) = self.next_pos(t, p) {
            if self.cls(t, n, space_only) != cls || n.row != p.row && n.col == 0 && cls == 0 {
                break;
            }
            p = n;
        }
        self.set_abs(t, p);
    }

    fn prev_word(&mut self, t: &Term, space_only: bool) {
        let mut p = self.abs_cursor();
        if let Some(n) = self.prev_pos(t, p) {
            p = n;
        }
        while self.char_at(t, p) == ' ' {
            match self.prev_pos(t, p) {
                Some(n) => p = n,
                None => break,
            }
        }
        let cls = self.cls(t, p, space_only);
        while let Some(n) = self.prev_pos(t, p) {
            if self.cls(t, n, space_only) != cls {
                break;
            }
            p = n;
        }
        self.set_abs(t, p);
    }

    fn sel_range(&self) -> Option<(Pos, Pos)> {
        let a = self.sel?;
        let b = self.abs_cursor();
        Some(if (a.row, a.col) <= (b.row, b.col) { (a, b) } else { (b, a) })
    }

    /// Text of the current selection.
    pub fn selection_text(&mut self, t: &Term) -> Option<String> {
        let (s, e) = self.sel_range()?;
        let mut out = String::new();
        let (c0, c1) = (self.sel.unwrap().col.min(self.cx), self.sel.unwrap().col.max(self.cx));
        for r in s.row..=e.row {
            let (cells, wrapped) = self.row(t, r);
            let (x0, x1) = if self.line_sel {
                (0, cells.len())
            } else if self.rect {
                (c0, (c1 + 1).min(cells.len()))
            } else {
                let x0 = if r == s.row { s.col } else { 0 };
                let x1 = if r == e.row { (e.col + 1).min(cells.len()) } else { cells.len() };
                (x0, x1)
            };
            let mut line = String::new();
            for c in &cells[x0.min(cells.len())..x1.max(x0).min(cells.len())] {
                push_cell_text(&mut line, c, &self.clusters);
            }
            let join = wrapped && !self.rect && !self.line_sel && r != e.row;
            if !join {
                let trimmed = line.trim_end().len();
                line.truncate(trimmed);
            }
            out.push_str(&line);
            if r != e.row && !join {
                out.push('\n');
            }
        }
        if self.line_sel {
            out.push('\n');
        }
        Some(out)
    }

    fn search(&mut self, t: &Term, needle: &str, forward: bool, skip_current: bool) -> bool {
        if needle.is_empty() {
            return false;
        }
        let total = self.total_rows(t);
        let start = self.abs_cursor();
        let lower = needle.chars().all(|c| !c.is_uppercase());
        let n = if lower { needle.to_lowercase() } else { needle.to_string() };
        let mut r = start.row;
        for step in 0..=total {
            let (text, _, cols) = self.row_text(t, r);
            let hay = if lower { text.to_lowercase() } else { text };
            let mut found: Option<usize> = None;
            if forward {
                let from_col = if step == 0 { start.col + usize::from(skip_current) } else { 0 };
                for (bi, _) in hay.match_indices(&n) {
                    let col = cols.get(bi).copied().unwrap_or(0);
                    if col >= from_col {
                        found = Some(col);
                        break;
                    }
                }
            } else {
                let lim = if step == 0 { start.col } else { usize::MAX };
                for (bi, _) in hay.match_indices(&n) {
                    let col = cols.get(bi).copied().unwrap_or(0);
                    if col < lim || (step == 0 && !skip_current && col == lim) {
                        found = Some(col);
                    }
                }
            }
            if let Some(col) = found {
                self.set_abs(t, Pos { row: r, col });
                return true;
            }
            if forward {
                r = (r + 1) % total;
            } else {
                r = if r == 0 { total - 1 } else { r - 1 };
            }
        }
        false
    }

    fn jump(&mut self, t: &Term, c: char, kind: char) {
        let p = self.abs_cursor();
        let (text, _, cols) = self.row_text(t, p.row);
        let chars: Vec<(usize, char)> = text.char_indices().map(|(bi, ch)| (cols.get(bi).copied().unwrap_or(0), ch)).collect();
        let target = match kind {
            'f' => chars.iter().find(|(x, ch)| *x > p.col && *ch == c).map(|(x, _)| *x),
            't' => chars.iter().find(|(x, ch)| *x > p.col + 1 && *ch == c).map(|(x, _)| x - 1),
            'F' => chars.iter().rev().find(|(x, ch)| *x < p.col && *ch == c).map(|(x, _)| *x),
            'T' => chars.iter().rev().find(|(x, ch)| *x + 1 < p.col && *ch == c).map(|(x, _)| x + 1),
            _ => None,
        };
        if let Some(x) = target {
            self.cx = x;
        }
    }

    fn scroll(&mut self, t: &Term, delta: isize) {
        let rows = t.rows();
        let total = self.total_rows(t);
        let max_top = total.saturating_sub(rows);
        let new_top = (self.top as isize + delta).clamp(0, max_top as isize) as usize;
        let abs = self.abs_cursor();
        self.top = new_top;
        // keep cursor on the same content row when possible
        if abs.row < self.top {
            self.cy = 0;
        } else if abs.row >= self.top + rows {
            self.cy = rows - 1;
        } else {
            self.cy = abs.row - self.top;
        }
    }

    fn at_bottom(&self, t: &Term) -> bool {
        self.top + t.rows() >= self.total_rows(t)
    }

    /// Execute a copy-mode command (tmux `send -X` names).
    pub fn command(&mut self, t: &Term, args: &[String]) -> Outcome {
        if !self.active {
            return Outcome::None;
        }
        self.sync(t);
        self.dirty = true;
        self.message = None;
        let rows = t.rows();
        let cols = t.cols();
        let cmd = args.first().map(|s| s.as_str()).unwrap_or("");
        let arg = args.get(1).cloned().unwrap_or_default();
        let count = 1usize;
        match cmd {
            "cancel" => {
                self.exit();
                return Outcome::Exit;
            }
            "cursor-left" => self.cx = self.cx.saturating_sub(count),
            "cursor-right" => self.cx = (self.cx + count).min(cols - 1),
            "cursor-up" => {
                if self.cy > 0 {
                    self.cy -= 1;
                } else {
                    self.scroll(t, -1);
                }
            }
            "cursor-down" => {
                if self.cy + 1 < rows {
                    self.cy += 1;
                } else {
                    self.scroll(t, 1);
                }
            }
            "start-of-line" => self.cx = 0,
            "back-to-indentation" => {
                let r = self.top + self.cy;
                let (cells, _) = self.row(t, r);
                self.cx = cells.iter().position(|c| !c.is_default_blank()).unwrap_or(0);
            }
            "end-of-line" => {
                let r = self.top + self.cy;
                self.cx = self.line_len(t, r).saturating_sub(1);
            }
            "next-word" => self.next_word(t, false),
            "next-space" => self.next_word(t, true),
            "next-word-end" => self.next_word_end(t, false),
            "next-space-end" => self.next_word_end(t, true),
            "previous-word" => self.prev_word(t, false),
            "previous-space" => self.prev_word(t, true),
            "top-line" => self.cy = 0,
            "middle-line" => self.cy = rows / 2,
            "bottom-line" => self.cy = rows - 1,
            "history-top" => {
                self.top = 0;
                self.cy = 0;
                self.cx = 0;
            }
            "history-bottom" => {
                let total = self.total_rows(t);
                self.top = total.saturating_sub(rows);
                self.cy = rows - 1;
                let r = self.top + self.cy;
                self.cx = self.line_len(t, r).saturating_sub(1).min(cols - 1);
            }
            "page-up" => self.scroll(t, -(rows as isize)),
            "page-down" => self.scroll(t, rows as isize),
            "halfpage-up" => self.scroll(t, -((rows / 2).max(1) as isize)),
            "halfpage-down" => self.scroll(t, (rows / 2).max(1) as isize),
            "scroll-up" => {
                let n = arg.parse::<isize>().unwrap_or(1);
                self.scroll(t, -n);
            }
            "scroll-down" | "scroll-down-and-cancel" => {
                let n = arg.parse::<isize>().unwrap_or(1);
                self.scroll(t, n);
                if (cmd == "scroll-down-and-cancel" || self.scroll_exit) && self.at_bottom(t) {
                    self.exit();
                    return Outcome::Exit;
                }
            }
            "page-down-and-cancel" => {
                self.scroll(t, rows as isize);
                if self.at_bottom(t) {
                    self.exit();
                    return Outcome::Exit;
                }
            }
            "goto-line" => {
                if let Ok(n) = arg.parse::<usize>() {
                    let total = self.total_rows(t);
                    let r = total.saturating_sub(rows).saturating_sub(n);
                    self.top = r;
                    self.cy = 0;
                }
            }
            "begin-selection" => {
                self.sel = Some(self.abs_cursor());
                self.line_sel = false;
            }
            "select-line" => {
                self.sel = Some(self.abs_cursor());
                self.line_sel = true;
            }
            "select-word" => {
                self.prev_word(t, false);
                self.sel = Some(self.abs_cursor());
                self.next_word_end(t, false);
            }
            "rectangle-toggle" => self.rect = !self.rect,
            "rectangle-on" => self.rect = true,
            "rectangle-off" => self.rect = false,
            "clear-selection" => {
                self.sel = None;
                self.line_sel = false;
            }
            "other-end" => {
                if let Some(a) = self.sel {
                    let cur = self.abs_cursor();
                    self.sel = Some(cur);
                    self.set_abs(t, a);
                }
            }
            "copy-selection" | "copy-selection-no-clear" | "copy-pipe" | "copy-pipe-no-clear" => {
                let pipe = if cmd.starts_with("copy-pipe") && !arg.is_empty() { Some(arg.clone()) } else { None };
                if let Some(text) = self.selection_text(t) {
                    if !cmd.ends_with("no-clear") {
                        self.sel = None;
                    }
                    return Outcome::Copy(text, false, pipe);
                }
            }
            "copy-selection-and-cancel" | "copy-pipe-and-cancel" => {
                let pipe = if cmd.starts_with("copy-pipe") && !arg.is_empty() { Some(arg.clone()) } else { None };
                let text = self.selection_text(t);
                self.exit();
                return match text {
                    Some(text) => Outcome::Copy(text, true, pipe),
                    None => Outcome::Exit,
                };
            }
            "copy-end-of-line" | "copy-end-of-line-and-cancel" => {
                let r = self.top + self.cy;
                let len = self.line_len(t, r);
                self.sel = Some(self.abs_cursor());
                self.cx = len.saturating_sub(1).max(self.cx);
                let text = self.selection_text(t).unwrap_or_default();
                self.sel = None;
                if cmd.ends_with("cancel") {
                    self.exit();
                    return Outcome::Copy(text, true, None);
                }
                return Outcome::Copy(text, false, None);
            }
            "copy-line" | "copy-line-and-cancel" => {
                self.sel = Some(self.abs_cursor());
                self.line_sel = true;
                let text = self.selection_text(t).unwrap_or_default();
                self.sel = None;
                self.line_sel = false;
                if cmd.ends_with("cancel") {
                    self.exit();
                    return Outcome::Copy(text, true, None);
                }
                return Outcome::Copy(text, false, None);
            }
            "search-forward" | "search-forward-text" | "search-forward-incremental" => {
                self.search = Some((arg.clone(), true));
                if !self.search(t, &arg, true, true) {
                    self.message = Some(format!("Pattern not found: {}", arg));
                }
            }
            "search-backward" | "search-backward-text" | "search-backward-incremental" => {
                self.search = Some((arg.clone(), false));
                if !self.search(t, &arg, false, true) {
                    self.message = Some(format!("Pattern not found: {}", arg));
                }
            }
            "search-again" | "search-reverse" => {
                if let Some((s, fwd)) = self.search.clone() {
                    let dir = if cmd == "search-again" { fwd } else { !fwd };
                    if !self.search(t, &s, dir, true) {
                        self.message = Some(format!("Pattern not found: {}", s));
                    }
                }
            }
            "jump-forward" | "jump-backward" | "jump-to-forward" | "jump-to-backward" => {
                if let Some(c) = arg.chars().next() {
                    let kind = match cmd {
                        "jump-forward" => 'f',
                        "jump-backward" => 'F',
                        "jump-to-forward" => 't',
                        _ => 'T',
                    };
                    self.jump = Some((c, kind));
                    self.jump(t, c, kind);
                }
            }
            "jump-again" | "jump-reverse" => {
                if let Some((c, k)) = self.jump {
                    let k = if cmd == "jump-again" {
                        k
                    } else {
                        match k {
                            'f' => 'F',
                            'F' => 'f',
                            't' => 'T',
                            _ => 't',
                        }
                    };
                    self.jump(t, c, k);
                }
            }
            "previous-prompt" | "next-prompt" => {
                let fwd = cmd == "next-prompt";
                let start = self.top + self.cy;
                let total = self.total_rows(t);
                let mut r = start;
                loop {
                    if fwd {
                        r += 1;
                        if r >= total {
                            break;
                        }
                    } else {
                        if r == 0 {
                            break;
                        }
                        r -= 1;
                    }
                    if self.row_is_prompt(t, r) {
                        self.set_abs(t, Pos { row: r, col: 0 });
                        break;
                    }
                }
            }
            "mouse-down" | "cursor-to" => {
                // args: x y (view coordinates)
                let x = arg.parse::<usize>().unwrap_or(0);
                let y = args.get(2).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                self.cx = x.min(cols - 1);
                self.cy = y.min(rows - 1);
                if cmd == "mouse-down" {
                    self.sel = None;
                }
            }
            "mouse-drag" => {
                let x = arg.parse::<usize>().unwrap_or(0);
                let y = args.get(2).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                if self.sel.is_none() {
                    self.sel = Some(self.abs_cursor());
                    self.line_sel = false;
                }
                if y == 0 && self.top > 0 {
                    self.scroll(t, -1);
                } else if y + 1 >= rows {
                    self.scroll(t, 1);
                }
                self.cx = x.min(cols - 1);
                self.cy = y.min(rows - 1);
            }
            _ => {
                self.message = Some(format!("unknown command: {}", cmd));
            }
        }
        Outcome::None
    }

    fn row_is_prompt(&mut self, t: &Term, r: usize) -> bool {
        if r >= self.hist_rows {
            let y = r - self.hist_rows;
            return y < t.rows() && t.row(y).mark & history::LINE_PROMPT != 0;
        }
        // first row of a history line carrying the prompt flag
        let mut base = 0usize;
        for (i, &n) in self.line_rows.iter().enumerate() {
            if base == r {
                return t.hist.line(i).map(history::line_flags).unwrap_or(0) & history::LINE_PROMPT != 0;
            }
            base += n as usize;
            if base > r {
                return false;
            }
        }
        false
    }

    /// Render the view: rows of cells with selection highlighted and a
    /// position indicator in the top-right corner.
    pub fn render(&mut self, t: &Term) -> Vec<(Vec<Cell>, bool)> {
        self.sync(t);
        let rows = t.rows();
        let cols = t.cols();
        let total = self.total_rows(t);
        self.top = self.top.min(total.saturating_sub(rows));
        let range = self.sel_range();
        let (rc0, rc1) = self.sel.map(|a| (a.col.min(self.cx), a.col.max(self.cx))).unwrap_or((0, 0));
        let sel_style = Style { fg: color_idx(0), bg: color_idx(3) };
        let mut out = Vec::with_capacity(rows);
        for y in 0..rows {
            let r = self.top + y;
            let (mut cells, wrapped) = self.row(t, r);
            if let Some((s, e)) = range
                && r >= s.row
                && r <= e.row
            {
                let (x0, x1) = if self.line_sel {
                    (0, cols)
                } else if self.rect {
                    (rc0, rc1 + 1)
                } else {
                    (if r == s.row { s.col } else { 0 }, if r == e.row { e.col + 1 } else { cols })
                };
                for c in cells.iter_mut().take(x1.min(cols)).skip(x0) {
                    c.fg = (c.fg & !COLOR_MASK) | sel_style.fg;
                    c.bg = (c.bg & !COLOR_MASK) | sel_style.bg;
                }
            }
            out.push((cells, wrapped));
        }
        // Position indicator "[pos/total]" like tmux.
        let from_bottom = total.saturating_sub(self.top + rows);
        let label = match &self.message {
            Some(m) => m.clone(),
            None => format!("[{}/{}]", from_bottom, self.hist_rows),
        };
        let lw = label.chars().count();
        if lw < cols && rows > 0 {
            let x0 = cols - lw;
            let row = &mut out[0].0;
            for (i, ch) in label.chars().enumerate() {
                row[x0 + i] = Cell { ch: ch as u32, fg: color_idx(0), bg: color_idx(3) };
            }
            if x0 > 0 && row[x0 - 1].ch != WIDE_SPACER && cell_width(&row[x0 - 1], &self.clusters) == 2 {
                row[x0 - 1] = Cell::EMPTY;
            }
        }
        self.dirty = false;
        out
    }

    pub fn position(&self, t: &Term) -> (usize, usize) {
        let total = self.total_rows(t);
        (total.saturating_sub(self.top + t.rows()), self.hist_rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::Emulator;

    fn emu_with_lines(n: usize) -> Emulator {
        let mut e = Emulator::new(20, 5, 1000);
        for i in 0..n {
            e.feed(format!("line {}\r\n", i).as_bytes());
        }
        e
    }

    #[test]
    fn scroll_and_copy() {
        let e = emu_with_lines(30);
        let mut c = CopyMode::new();
        c.enter(&e.term, false);
        assert_eq!(c.total_rows(&e.term), 26 + 5);
        c.command(&e.term, &["history-top".into()]);
        let view = c.render(&e.term);
        let first: String = view[0].0.iter().map(|c| char::from_u32(c.ch).unwrap_or(' ')).collect::<String>().replace('\0', " ");
        assert!(first.starts_with("line 0"), "{first:?}");
        c.command(&e.term, &["begin-selection".into()]);
        c.command(&e.term, &["cursor-down".into()]);
        c.command(&e.term, &["end-of-line".into()]);
        match c.command(&e.term, &["copy-selection-and-cancel".into()]) {
            Outcome::Copy(t, true, None) => assert_eq!(t, "line 0\nline 1"),
            _ => panic!("expected copy"),
        }
        assert!(!c.active);
    }

    #[test]
    fn search_finds_history() {
        let e = emu_with_lines(30);
        let mut c = CopyMode::new();
        c.enter(&e.term, false);
        c.command(&e.term, &["search-backward".into(), "line 3".into()]);
        let p = c.abs_cursor();
        let (text, _, _) = c.row_text(&e.term, p.row);
        assert!(text.starts_with("line 3"), "{text}");
    }

    #[test]
    fn view_stable_while_output_arrives() {
        let mut e = emu_with_lines(30);
        let mut c = CopyMode::new();
        c.enter(&e.term, false);
        c.command(&e.term, &["history-top".into()]);
        let before = c.render(&e.term)[0].0[..10].to_vec();
        for i in 0..10 {
            e.feed(format!("more {}\r\n", i).as_bytes());
        }
        let after = c.render(&e.term)[0].0[..10].to_vec();
        assert_eq!(before, after);
        assert_eq!(c.position(&e.term), (36, 36));
    }
}
