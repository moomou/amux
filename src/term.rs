//! Terminal emulator: a `vte::Perform` implementation over a compact grid.
//!
//! Semantics follow tmux/xterm closely (pending-wrap, BCE, scroll regions,
//! alternate screen, reflow on resize). Responses to queries (DA, DSR, OSC
//! color queries, DECRQM) are queued in `out` so the pane owner can answer
//! the application directly, even while no server is attached.

use crate::cell::*;
use crate::history::{self, History};

#[derive(Clone, Debug)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub wrapped: bool,
    pub mark: u8,
    /// Cells written so far (tmux `cellused`).
    pub used: u16,
    /// Allocated cells (tmux `cellsize`, rounded to 1/4, 1/2 or full width);
    /// wrapped rows join up to here.
    pub size: u16,
}

impl Row {
    pub fn new(cols: usize, blank: Cell) -> Row {
        let mut r = Row { cells: vec![blank; cols], wrapped: false, mark: 0, used: 0, size: 0 };
        if blank.bg != 0 {
            r.size = cols as u16;
        }
        r
    }
    /// End of the cells that may be non-empty: nothing is ever written past
    /// tmux's cellsize without growing it.
    #[inline]
    fn extent(&self) -> usize {
        let e = (self.size.max(self.used) as usize).min(self.cells.len());
        debug_assert!(self.cells[e..].iter().all(|c| *c == Cell::EMPTY), "row cells beyond extent must be empty");
        e
    }
    fn reset(&mut self, cols: usize, blank: Cell) {
        if blank == Cell::EMPTY && self.cells.len() == cols {
            let e = self.extent();
            self.cells[..e].fill(Cell::EMPTY);
        } else {
            self.cells.clear();
            self.cells.resize(cols, blank);
        }
        self.wrapped = false;
        self.mark = 0;
        self.used = 0;
        self.size = if blank.bg & COLOR_MASK != 0 { cols as u16 } else { 0 };
    }
    /// tmux `grid_expand_line` rounding.
    #[inline]
    fn grow(&mut self, end: usize) {
        if end <= self.size as usize {
            return;
        }
        let sx = self.cells.len();
        let n = if end < sx / 4 {
            sx / 4
        } else if end < sx / 2 {
            sx / 2
        } else {
            sx.max(end)
        };
        self.size = n.min(sx).min(u16::MAX as usize) as u16;
    }
    #[inline]
    fn touch(&mut self, end: usize) {
        if end > self.used as usize {
            self.used = end.min(u16::MAX as usize) as u16;
        }
        self.grow(end);
    }
    /// Push this row into history: wrapped rows keep written cells plus virtual
    /// padding up to the allocated size (tmux cellused/cellsize).
    pub fn push_to(&self, hist: &mut History, clusters: &Clusters) {
        if self.wrapped {
            let used = (self.used as usize).min(self.cells.len());
            let size = (self.size as usize).min(self.cells.len());
            hist.push_row_pad(&self.cells[..used], size.saturating_sub(used), true, self.mark, clusters);
        } else {
            hist.push_row(self.line_cells(), false, self.mark, clusters);
        }
    }

    /// Cells that belong to the logical line: written cells for wrapped rows,
    /// otherwise everything up to the last non-blank cell.
    pub fn line_cells(&self) -> &[Cell] {
        if self.wrapped {
            &self.cells[..(self.size as usize).min(self.cells.len())]
        } else {
            let mut end = self.extent();
            while end > 0 && self.cells[end - 1].is_default_blank() {
                end -= 1;
            }
            &self.cells[..end]
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Charset {
    Ascii,
    Dec,
}

#[derive(Clone, Copy, Debug)]
struct Cursor {
    x: usize,
    y: usize,
    pen: Style,
    pending: bool,
}

#[derive(Clone, Copy, Debug)]
struct Saved {
    x: usize,
    y: usize,
    pen: Style,
    origin: bool,
    g: [Charset; 2],
    gl: usize,
}

pub const MOUSE_NONE: u16 = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Modes {
    pub insert: bool,
    pub autowrap: bool,
    pub origin: bool,
    pub lnm: bool,
    pub app_cursor: bool,
    pub app_keypad: bool,
    pub cursor_visible: bool,
    pub cursor_blink: bool,
    pub cursor_shape: u8,
    /// 0 none, 9 x10, 1000 normal, 1002 button, 1003 any
    pub mouse: u16,
    /// 0 default, 1005 utf8, 1006 sgr, 1015 urxvt
    pub mouse_enc: u16,
    pub focus: bool,
    pub bracketed_paste: bool,
    pub sync: bool,
    pub alt_scroll: bool,
    pub reverse: bool,
    pub alt_screen: bool,
}

impl Default for Modes {
    fn default() -> Modes {
        Modes {
            insert: false,
            autowrap: true,
            origin: false,
            lnm: false,
            app_cursor: false,
            app_keypad: false,
            cursor_visible: true,
            cursor_blink: false,
            cursor_shape: 0,
            mouse: 0,
            mouse_enc: 0,
            focus: false,
            bracketed_paste: false,
            sync: false,
            alt_scroll: false,
            reverse: false,
            alt_screen: false,
        }
    }
}

impl Modes {
    pub fn to_bits(&self) -> u32 {
        let mut b = 0u32;
        let flags = [
            self.insert,
            self.autowrap,
            self.origin,
            self.lnm,
            self.app_cursor,
            self.app_keypad,
            self.cursor_visible,
            self.cursor_blink,
            self.focus,
            self.bracketed_paste,
            self.sync,
            self.alt_scroll,
            self.reverse,
            self.alt_screen,
        ];
        for (i, f) in flags.iter().enumerate() {
            if *f {
                b |= 1 << i;
            }
        }
        b
    }
    pub fn from_bits(b: u32, cursor_shape: u8, mouse: u16, mouse_enc: u16) -> Modes {
        let f = |i: u32| b & (1 << i) != 0;
        Modes {
            insert: f(0),
            autowrap: f(1),
            origin: f(2),
            lnm: f(3),
            app_cursor: f(4),
            app_keypad: f(5),
            cursor_visible: f(6),
            cursor_blink: f(7),
            focus: f(8),
            bracketed_paste: f(9),
            sync: f(10),
            alt_scroll: f(11),
            reverse: f(12),
            alt_screen: f(13),
            cursor_shape,
            mouse,
            mouse_enc,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Bell,
    Title(String),
    Cwd(String),
    Notify(String, String),
    /// OSC 9;4 progress: state (0 clear,1 set,2 error,3 indeterminate,4 paused), percent
    Progress(u8, u8),
    /// OSC 52 payload (base64, as sent)
    Clipboard(String),
    /// OSC 133 semantic prompt mark ('A','B','C','D') and exit status for 'D'
    Prompt(u8, Option<i32>),
}

#[derive(Default)]
struct Dcs {
    active: bool,
    kind: u8,
    buf: Vec<u8>,
}

pub type Rgb = (u8, u8, u8);

pub struct Term {
    cols: usize,
    rows: usize,
    grid: Vec<Row>,
    other: Option<Vec<Row>>,
    in_alt: bool,
    pub hist: History,
    cur: Cursor,
    saved: Option<Saved>,
    alt_saved: Option<(usize, usize, bool, Style)>,
    top: usize,
    bot: usize,
    tabs: Vec<bool>,
    pub modes: Modes,
    g: [Charset; 2],
    gl: usize,
    last_char: Option<char>,
    /// Inside a run of plain ASCII (tmux "collect" fast path).
    run_active: bool,
    run_y: usize,
    run_end: usize,
    run_size0: u16,
    /// Printable ASCII from vte not yet written (see `flush_ascii`).
    abuf: Vec<u8>,
    pub title: String,
    title_stack: Vec<String>,
    pub cwd: Option<String>,
    pub clusters: Clusters,
    dirty: Vec<u64>,
    pub all_dirty: bool,
    /// Bytes to write back to the application (query responses).
    pub out: Vec<u8>,
    pub events: Vec<Event>,
    dcs: Dcs,
    /// Default fg/bg colors of the outer terminal (for OSC 10/11 queries).
    pub default_colors: Option<(Rgb, Rgb)>,
    /// Number of rows scrolled into history (monotonic).
    pub scrolled: u64,
    /// Rows scrolled into history that a taller resize may pull back (tmux `hscrolled`).
    hscrolled: usize,
    /// Total bytes processed (monotonic).
    pub bytes_in: u64,
}

pub fn dec_special(c: char) -> char {
    match c {
        '`' => '◆',
        'a' => '▒',
        'b' => '␉',
        'c' => '␌',
        'd' => '␍',
        'e' => '␊',
        'f' => '°',
        'g' => '±',
        'h' => '␤',
        'i' => '␋',
        'j' => '┘',
        'k' => '┐',
        'l' => '┌',
        'm' => '└',
        'n' => '┼',
        'o' => '⎺',
        'p' => '⎻',
        'q' => '─',
        'r' => '⎼',
        's' => '⎽',
        't' => '├',
        'u' => '┤',
        'v' => '┴',
        'w' => '┬',
        'x' => '│',
        'y' => '≤',
        'z' => '≥',
        '{' => 'π',
        '|' => '≠',
        '}' => '£',
        '~' => '·',
        c => c,
    }
}

fn default_tabs(cols: usize) -> Vec<bool> {
    (0..cols).map(|i| i % 8 == 0 && i != 0).collect()
}

/// First subparameter of param `i`, or `def` when missing/zero.
fn arg(params: &vte::Params, i: usize, def: usize) -> usize {
    match params.iter().nth(i) {
        Some(p) if !p.is_empty() && p[0] != 0 => p[0] as usize,
        _ => def,
    }
}

/// First subparameter of param `i` allowing zero.
fn arg0(params: &vte::Params, i: usize) -> Option<u16> {
    params.iter().nth(i).and_then(|p| p.first().copied())
}

impl Term {
    pub fn new(cols: usize, rows: usize, history_limit: usize) -> Term {
        let cols = cols.clamp(2, 4096);
        let rows = rows.clamp(1, 4096);
        Term {
            cols,
            rows,
            grid: (0..rows).map(|_| Row::new(cols, Cell::EMPTY)).collect(),
            other: None,
            in_alt: false,
            hist: History::new(history_limit),
            cur: Cursor { x: 0, y: 0, pen: Style::DEFAULT, pending: false },
            saved: None,
            alt_saved: None,
            top: 0,
            bot: rows - 1,
            tabs: default_tabs(cols),
            modes: Modes::default(),
            g: [Charset::Ascii; 2],
            gl: 0,
            last_char: None,
            run_active: false,
            run_y: 0,
            run_end: 0,
            run_size0: 0,
            abuf: Vec::with_capacity(512),
            title: String::new(),
            title_stack: Vec::new(),
            cwd: None,
            clusters: Clusters::default(),
            dirty: vec![!0u64; rows.div_ceil(64)],
            all_dirty: true,
            out: Vec::new(),
            events: Vec::new(),
            dcs: Dcs::default(),
            default_colors: None,
            scrolled: 0,
            hscrolled: 0,
            bytes_in: 0,
        }
    }

    pub fn cols(&self) -> usize {
        self.cols
    }
    pub fn rows(&self) -> usize {
        self.rows
    }
    pub fn row(&self, y: usize) -> &Row {
        &self.grid[y]
    }
    pub fn grid(&self) -> &[Row] {
        &self.grid
    }
    pub fn cursor(&self) -> (usize, usize) {
        (self.cur.x, self.cur.y)
    }
    pub fn cursor_pending(&self) -> bool {
        self.cur.pending
    }
    pub fn in_alt(&self) -> bool {
        self.in_alt
    }
    pub fn scroll_region(&self) -> (usize, usize) {
        (self.top, self.bot)
    }

    #[inline]
    fn mark_dirty(&mut self, y: usize) {
        if let Some(w) = self.dirty.get_mut(y >> 6) {
            *w |= 1 << (y & 63);
        }
    }
    fn mark_range(&mut self, a: usize, b: usize) {
        let b = b.min(self.rows - 1);
        if a > b {
            return;
        }
        // Set bits a..=b word by word.
        let mut y = a;
        while y <= b {
            let (w, bit) = (y >> 6, y & 63);
            let n = (b - y + 1).min(64 - bit);
            let mask = if n == 64 { !0u64 } else { ((1u64 << n) - 1) << bit };
            if let Some(word) = self.dirty.get_mut(w) {
                *word |= mask;
            }
            y += n;
        }
    }
    pub fn mark_all(&mut self) {
        self.all_dirty = true;
        self.dirty.fill(!0);
    }
    pub fn is_dirty(&self) -> bool {
        self.all_dirty || self.dirty.iter().any(|&w| w != 0)
    }
    /// Collect and clear dirty row indices.
    pub fn take_dirty(&mut self) -> Vec<usize> {
        let mut v = Vec::new();
        for (i, w) in self.dirty.iter_mut().enumerate() {
            let mut bits = *w;
            while bits != 0 {
                let b = bits.trailing_zeros() as usize;
                let y = i * 64 + b;
                if y < self.rows {
                    v.push(y);
                }
                bits &= bits - 1;
            }
            *w = 0;
        }
        self.all_dirty = false;
        v
    }

    pub fn feed(&mut self, parser: &mut vte::Parser, bytes: &[u8]) {
        self.bytes_in += bytes.len() as u64;
        parser.advance(self, bytes);
        self.flush_ascii();
        self.end_run();
    }

    /// Finish an ASCII run: clear orphaned wide-char halves after it.
    fn end_run(&mut self) {
        if !self.run_active {
            return;
        }
        self.run_active = false;
        let y = self.run_y;
        if y >= self.rows {
            return;
        }
        let mut xx = self.run_end;
        while xx < self.cols && self.grid[y].cells[xx].ch == WIDE_SPACER {
            self.grid[y].cells[xx] = Cell::EMPTY;
            self.grid[y].grow(xx + 1);
            xx += 1;
        }
    }

    #[inline]
    fn erase_cell(&self) -> Cell {
        Cell::blank_bg(self.cur.pen.bg)
    }

    /// Cursor x in tmux terms: `cols` while a wrap is pending.
    #[inline]
    fn tx(&self) -> usize {
        if self.cur.pending { self.cols } else { self.cur.x }
    }

    /// Write buffered printable ASCII. Equivalent to `put_char` per byte, but
    /// the rest of a row is written by `continue_run` in one tight loop.
    fn flush_ascii(&mut self) {
        if self.abuf.is_empty() {
            return;
        }
        let buf = std::mem::take(&mut self.abuf);
        let mut i = 0;
        while i < buf.len() {
            self.put_char(buf[i] as char);
            i += 1;
            i += self.continue_run(&buf[i..]);
        }
        self.abuf = buf;
        self.abuf.clear();
    }

    /// Extend the active ASCII run along the current row: exactly what the
    /// `put_char` fast path does for each byte, stopping at the last column.
    fn continue_run(&mut self, bytes: &[u8]) -> usize {
        if !self.run_active || self.cur.pending || self.cur.y != self.run_y || bytes.is_empty() {
            return 0;
        }
        let (x0, y, cols) = (self.cur.x, self.cur.y, self.cols);
        let n = bytes.len().min(cols - x0);
        let pen = self.cur.pen;
        let size0 = self.run_size0;
        let r = &mut self.grid[y];
        for (cell, &b) in r.cells[x0..x0 + n].iter_mut().zip(&bytes[..n]) {
            *cell = Cell { ch: b as u32, fg: pen.fg, bg: pen.bg };
        }
        let end = x0 + n;
        if end > r.used as usize {
            r.used = end as u16;
        }
        r.size = size0;
        r.grow(end);
        self.run_end = end;
        self.mark_dirty(y);
        self.last_char = Some(bytes[n - 1] as char);
        if end >= cols {
            self.cur.x = cols - 1;
            self.cur.pending = true;
        } else {
            self.cur.x = end;
        }
        n
    }

    fn put_char(&mut self, c: char) {
        // tmux only remembers plain ASCII for REP.
        let orig = c;
        let dec = self.g[self.gl] == Charset::Dec;
        // tmux writes plain ASCII through a "collect" fast path whose wide-char
        // cleanup only happens at run boundaries.
        let fast = !dec && (0x20..0x7f).contains(&(c as u32)) && self.modes.autowrap && !self.modes.insert;
        let c = if dec && (c as u32) < 0x7f { dec_special(c) } else { c };
        let w = char_width(c);
        if w == 0 {
            self.end_run();
            self.last_char = None;
            self.combine(c);
            return;
        }
        let cols = self.cols;
        if !fast {
            self.end_run();
        }
        if !self.modes.autowrap && w > 1 && (w > cols || (!self.cur.pending && self.cur.x > cols - w)) {
            return;
        }
        if self.modes.insert && !self.cur.pending {
            self.insert_cells_raw(self.cur.x, w);
        }
        if self.modes.autowrap && self.tx() > cols - w {
            self.end_run();
            let y = self.cur.y;
            self.grid[y].wrapped = true;
            self.linefeed_with(Cell::EMPTY);
            self.cur.x = 0;
            self.cur.pending = false;
        }
        let (x, y) = (self.cur.x, self.cur.y);
        if self.tx() > cols - w {
            return;
        }
        let pen = self.cur.pen;
        let cell = Cell { ch: c as u32, fg: pen.fg, bg: pen.bg };
        if fast {
            if !self.run_active {
                if x != 0 {
                    // Port of tmux screen_write_collect_end's backward scan.
                    let clusters = &self.clusters;
                    let r = &mut self.grid[y];
                    let mut xx = x;
                    let mut wide = false;
                    while xx > 0 {
                        if r.cells[xx].ch != WIDE_SPACER {
                            wide = r.cells[xx].ch != BLANK && cell_width(&r.cells[xx], clusters) > 1;
                            break;
                        }
                        r.cells[xx] = Cell::EMPTY;
                        r.grow(xx + 1);
                        xx -= 1;
                    }
                    if wide {
                        r.cells[xx] = Cell::EMPTY;
                        r.grow(xx + 1);
                    }
                }
                self.run_active = true;
                self.run_y = y;
                self.run_size0 = self.grid[y].size;
            }
            let size0 = self.run_size0;
            let r = &mut self.grid[y];
            r.cells[x] = cell;
            if x + 1 > r.used as usize {
                r.used = (x + 1) as u16;
            }
            r.size = size0;
            r.grow(x + 1);
            self.run_end = x + 1;
        } else {
            let clusters = &self.clusters;
            let r = &mut self.grid[y];
            // Overwriting part of a wide character clears the rest of it.
            let was_spacer = r.cells[x].ch == WIDE_SPACER;
            if was_spacer {
                let mut xx = x;
                while xx > 0 {
                    xx -= 1;
                    let sp = r.cells[xx].ch == WIDE_SPACER;
                    r.cells[xx] = Cell::EMPTY;
                    if !sp {
                        break;
                    }
                }
            }
            let cur_wide = !was_spacer && r.cells[x].ch != BLANK && cell_width(&r.cells[x], clusters) > 1;
            if w != 1 || cur_wide || was_spacer {
                let mut xx = x + w;
                while xx < cols && r.cells[xx].ch == WIDE_SPACER {
                    r.cells[xx] = Cell::EMPTY;
                    xx += 1;
                }
            }
            r.cells[x] = cell;
            if w == 2 {
                r.cells[x + 1] = Cell { ch: WIDE_SPACER, fg: pen.fg, bg: pen.bg };
            }
            r.touch(x + w);
        }
        self.mark_dirty(y);
        self.last_char = if (orig as u32) < 0x80 { Some(orig) } else { None };
        if self.modes.autowrap {
            let nx = x + w;
            if nx >= cols {
                self.cur.x = cols - 1;
                self.cur.pending = true;
            } else {
                self.cur.x = nx;
            }
        } else if x + w < cols {
            self.cur.x = x + w;
        } else {
            self.cur.x = cols - 1;
        }
    }

    fn combine(&mut self, c: char) {
        if (c as u32) < 0x20 {
            return;
        }
        let tx = self.tx();
        if tx == 0 {
            return;
        }
        let y = self.cur.y;
        let mut x = tx - 1;
        while self.grid[y].cells[x].ch == WIDE_SPACER {
            if x == 0 {
                return;
            }
            x -= 1;
        }
        let cell = self.grid[y].cells[x];
        let mut s = String::new();
        push_cell_text(&mut s, &cell, &self.clusters);
        if s.len() > 28 {
            return;
        }
        s.push(c);
        if self.clusters.len() >= MAX_CLUSTERS {
            self.gc_clusters();
        }
        let id = self.clusters.intern(&s);
        self.grid[y].cells[x].ch = id;
        self.mark_dirty(y);
    }

    fn gc_clusters(&mut self) {
        let old = std::mem::take(&mut self.clusters);
        let mut new = Clusters::default();
        let mut remap = |rows: &mut Vec<Row>| {
            for r in rows.iter_mut() {
                for c in r.cells.iter_mut() {
                    if c.ch & CLUSTER_BIT != 0 {
                        c.ch = match old.get(c.ch) {
                            Some(s) => new.intern(s),
                            None => BLANK,
                        };
                    }
                }
            }
        };
        remap(&mut self.grid);
        if let Some(o) = self.other.as_mut() {
            remap(o);
        }
        self.clusters = new;
        self.mark_all();
    }

    /// Insert `n` blank cells at `x` (tmux: no wide-char fixups).
    fn insert_cells_raw(&mut self, x: usize, n: usize) {
        let y = self.cur.y;
        let cols = self.cols;
        let blank = self.erase_cell();
        let row = &mut self.grid[y].cells;
        if x >= cols - 1 {
            row[x.min(cols - 1)] = blank;
        } else if n >= cols - x {
            // tmux grid_move_cells moves nothing here (and clears nothing).
            return;
        } else {
            // Port of tmux grid_move_cells: only the moved-from cells are wiped.
            let moved = cols - x - n;
            for i in (0..moved).rev() {
                row[x + n + i] = row[x + i];
            }
            for c in &mut row[x..x + moved.min(n)] {
                *c = blank;
            }
            self.grid[y].touch(cols);
        }
        self.mark_dirty(y);
    }

    fn insert_cells(&mut self, n: usize) {
        if self.cur.pending {
            return;
        }
        let n = n.min(self.cols - self.cur.x);
        if n == 0 {
            return;
        }
        self.insert_cells_raw(self.cur.x, n);
    }

    fn delete_cells(&mut self, n: usize) {
        if self.cur.pending {
            return;
        }
        let (x, y) = (self.cur.x, self.cur.y);
        let cols = self.cols;
        let n = n.min(cols - x);
        if n == 0 {
            return;
        }
        let blank = self.erase_cell();
        let row = &mut self.grid[y].cells;
        row[x..].rotate_left(n);
        for c in &mut row[cols - n..] {
            *c = blank;
        }
        if n < cols - x {
            self.grid[y].touch(cols - n);
        }
        self.mark_dirty(y);
    }

    /// Erase [x0, x1) on row y. A full-width erase also resets line flags.
    fn erase_cells(&mut self, y: usize, x0: usize, x1: usize) {
        let x1 = x1.min(self.cols);
        if x0 >= x1 {
            return;
        }
        let blank = self.erase_cell();
        if x0 == 0 && x1 == self.cols {
            self.clear_lines(y, 1);
            return;
        } else {
            if blank.bg & COLOR_MASK != 0 {
                self.grid[y].grow(x1);
            }
            for c in &mut self.grid[y].cells[x0..x1] {
                *c = blank;
            }
        }
        self.mark_dirty(y);
    }

    /// Clear the screen; on the primary screen used rows go to history (tmux semantics).
    fn clear_screen(&mut self) {
        if self.in_alt {
            self.clear_lines(0, self.rows);
            return;
        }
        let mut last = self.rows;
        while last > 0 && self.grid[last - 1].used == 0 {
            last -= 1;
        }
        if last == 0 {
            self.clear_lines(0, self.rows);
            return;
        }
        let blank = self.erase_cell();
        let cols = self.cols;
        for y in 0..last {
            self.grid[y].push_to(&mut self.hist, &self.clusters);
        }
        self.scrolled += last as u64;
        self.hscrolled = 0;
        self.grid.rotate_left(last);
        let rows = self.rows;
        for r in &mut self.grid[rows - last..] {
            r.reset(cols, blank);
        }
        if last < self.rows {
            self.clear_lines(0, self.rows - last);
        }
        self.mark_all();
    }

    /// tmux `grid_clear_lines`: reset rows and unwrap the line above.
    fn clear_lines(&mut self, y: usize, n: usize) {
        if n == 0 || y >= self.rows {
            return;
        }
        let blank = self.erase_cell();
        let cols = self.cols;
        for yy in y..(y + n).min(self.rows) {
            self.grid[yy].reset(cols, blank);
            self.mark_dirty(yy);
        }
        self.unwrap_above(y);
    }

    #[inline]
    fn unwrap_above(&mut self, y: usize) {
        if y > 0 {
            self.grid[y - 1].wrapped = false;
        } else {
            self.hist.close();
        }
    }

    /// tmux `grid_move_lines`: move `ny` rows from `py` to `dy`, wiping vacated rows.
    fn move_lines(&mut self, dy: usize, py: usize, ny: usize) {
        if ny == 0 || py == dy || py + ny > self.rows || dy + ny > self.rows {
            return;
        }
        self.unwrap_above(dy);
        if dy > py {
            for i in (0..ny).rev() {
                self.grid.swap(py + i, dy + i);
            }
        } else {
            for i in 0..ny {
                self.grid.swap(py + i, dy + i);
            }
        }
        let blank = self.erase_cell();
        let cols = self.cols;
        for (yy, row) in self.grid.iter_mut().enumerate().take(py + ny).skip(py) {
            if yy < dy || yy >= dy + ny {
                row.reset(cols, blank);
            }
        }
        if py < dy || py >= dy + ny {
            self.unwrap_above(py);
        }
        self.mark_range(py.min(dy), (py.max(dy) + ny - 1).min(self.rows - 1));
    }

    /// tmux's `grid_clear(0, py, sx, ny)` where `ny` was computed with unsigned
    /// wrap-around: a "negative" count only unwraps the line above `py`.
    fn clear_lines_signed(&mut self, py: usize, ny: isize) {
        if ny > 0 {
            self.clear_lines(py, ny as usize);
        } else if ny < 0 && py < self.rows {
            self.unwrap_above(py);
        }
    }

    fn erase_rows(&mut self, y0: usize, y1: usize) {
        let y1 = y1.min(self.rows);
        if y0 < y1 {
            self.clear_lines(y0, y1 - y0);
        }
    }

    pub fn scroll_up(&mut self, n: usize) {
        let blank = self.erase_cell();
        self.scroll_up_with(n, blank);
    }

    fn scroll_up_with(&mut self, n: usize, blank: Cell) {
        let (top, bot) = (self.top, self.bot);
        let n = n.min(bot + 1 - top);
        if n == 0 {
            return;
        }
        if self.in_alt {
            for _ in 0..n {
                self.move_lines(top, top + 1, bot - top);
                if top == bot {
                    let cols = self.cols;
                    self.grid[top].reset(cols, blank);
                }
            }
            self.mark_range(top, bot);
            return;
        }
        for i in top..top + n {
            self.grid[i].push_to(&mut self.hist, &self.clusters);
        }
        self.scrolled += n as u64;
        if top == 0 && bot == self.rows - 1 {
            self.hscrolled += n;
        }
        let cols = self.cols;
        self.grid[top..=bot].rotate_left(n);
        for row in &mut self.grid[(bot + 1 - n)..=bot] {
            row.reset(cols, blank);
        }
        self.mark_range(top, bot);
    }

    pub fn scroll_down(&mut self, n: usize) {
        let (top, bot) = (self.top, self.bot);
        let n = n.min(bot + 1 - top);
        for _ in 0..n {
            if top == bot {
                self.clear_lines(top, 1);
            } else {
                self.move_lines(top + 1, top, bot - top);
            }
        }
    }

    /// IL at row y (tmux grid_view_insert_lines[_region]).
    fn insert_lines(&mut self, y: usize, n: usize) {
        if y < self.top || y > self.bot {
            let n = n.min(self.rows - y);
            self.move_lines(y + n, y, self.rows - y - n);
        } else {
            let n = n.min(self.bot + 1 - y);
            let ny2 = self.bot + 1 - y - n;
            self.move_lines(self.bot + 1 - ny2, y, ny2);
            self.clear_lines_signed(y + ny2, n as isize - ny2 as isize);
        }
    }

    /// DL at row y (tmux grid_view_delete_lines[_region]).
    fn delete_lines(&mut self, y: usize, n: usize) {
        if y < self.top || y > self.bot {
            let n = n.min(self.rows - y);
            let sy = self.rows;
            self.move_lines(y, y + n, sy - y - n);
            self.clear_lines_signed(sy - n, (y + n) as isize - (sy - n) as isize);
        } else {
            let n = n.min(self.bot + 1 - y);
            let ny2 = self.bot + 1 - y - n;
            self.move_lines(y, y + n, ny2);
            self.clear_lines_signed(y + ny2, n as isize - ny2 as isize);
        }
    }

    /// Line feed; keeps a pending wrap (tmux semantics).
    fn linefeed(&mut self) {
        let blank = self.erase_cell();
        self.linefeed_with(blank);
    }

    fn linefeed_with(&mut self, blank: Cell) {
        if self.cur.y == self.bot {
            self.scroll_up_with(1, blank);
        } else if self.cur.y + 1 < self.rows {
            self.cur.y += 1;
        }
    }

    fn reverse_index(&mut self) {
        if self.cur.y == self.top {
            self.scroll_down(1);
        } else if self.cur.y > 0 {
            self.cur.y -= 1;
        }
    }

    fn cr(&mut self) {
        self.cur.x = 0;
        self.cur.pending = false;
    }

    fn backspace(&mut self) {
        if self.cur.pending {
            self.cur.pending = false;
            return;
        }
        if self.cur.x == 0 {
            if self.cur.y > 0 && self.grid[self.cur.y - 1].wrapped {
                self.cur.y -= 1;
                self.cur.x = self.cols - 1;
            }
        } else {
            self.cur.x -= 1;
        }
    }

    fn tab(&mut self, n: usize) {
        for _ in 0..n {
            if self.tx() >= self.cols - 1 {
                break;
            }
            let mut x = self.cur.x;
            loop {
                x += 1;
                if self.tabs[x] || x >= self.cols - 1 {
                    break;
                }
            }
            self.cur.x = x;
        }
    }

    fn back_tab(&mut self, n: usize) {
        let mut x = self.cur.x;
        let mut n = n;
        while x > 0 && n > 0 {
            n -= 1;
            loop {
                x -= 1;
                if x == 0 || self.tabs[x] {
                    break;
                }
            }
        }
        self.cur.x = x;
        self.cur.pending = false;
    }

    fn goto(&mut self, x: usize, y: usize) {
        let (ymin, ymax) = if self.modes.origin { (self.top, self.bot) } else { (0, self.rows - 1) };
        self.cur.x = x.min(self.cols - 1);
        self.cur.y = (y + if self.modes.origin { self.top } else { 0 }).clamp(ymin, ymax);
        self.cur.pending = false;
    }

    fn move_up(&mut self, n: usize) {
        let min = if self.cur.y >= self.top { self.top } else { 0 };
        self.cur.y = self.cur.y.saturating_sub(n).max(min);
        self.cur.pending = false;
    }

    fn move_down(&mut self, n: usize) {
        let max = if self.cur.y <= self.bot { self.bot } else { self.rows - 1 };
        self.cur.y = (self.cur.y + n).min(max);
        self.cur.pending = false;
    }

    fn save_cursor(&mut self) {
        self.saved = Some(Saved { x: self.tx(), y: self.cur.y, pen: self.cur.pen, origin: self.modes.origin, g: self.g, gl: self.gl });
    }

    fn restore_cursor(&mut self) {
        let s = self.saved.unwrap_or(Saved { x: 0, y: 0, pen: Style::DEFAULT, origin: false, g: [Charset::Ascii; 2], gl: 0 });
        self.cur.pen = s.pen;
        self.g = s.g;
        self.gl = s.gl;
        self.modes.origin = s.origin;
        self.cur.x = s.x.min(self.cols - 1);
        self.cur.y = s.y.min(self.rows - 1);
        self.cur.pending = false;
    }

    fn enter_alt(&mut self, save_cursor: bool) {
        if self.in_alt {
            return;
        }
        if save_cursor {
            self.alt_saved = Some((self.cur.x, self.cur.y, self.cur.pending, self.cur.pen));
        }
        let cols = self.cols;
        let alt: Vec<Row> = (0..self.rows).map(|_| Row::new(cols, Cell::EMPTY)).collect();
        self.other = Some(std::mem::replace(&mut self.grid, alt));
        self.in_alt = true;
        self.modes.alt_screen = true;
        self.hist.close();
        self.mark_all();
    }

    fn leave_alt(&mut self, restore_cursor: bool) {
        if restore_cursor && let Some((x, y, p, pen)) = self.alt_saved {
            self.cur.x = x.min(self.cols - 1);
            self.cur.y = y.min(self.rows - 1);
            self.cur.pending = p && self.in_alt;
            self.cur.pen = pen;
        }
        if !self.in_alt {
            self.cur.pending = false;
            return;
        }
        if let Some(p) = self.other.take() {
            let (orows, ocols) = (p.len(), p.first().map(|r| r.cells.len()).unwrap_or(self.cols));
            if orows != self.rows || ocols != self.cols {
                let tx = if self.cur.pending { self.cols } else { self.cur.x };
                let (x, y) = (tx.min(ocols), self.cur.y.min(orows.saturating_sub(1)));
                let (pending, x) = if x >= ocols { (true, ocols - 1) } else { (false, x) };
                self.in_alt = false;
                let (g, nx, ny, np) = self.resize_primary(p, x, y, pending, ocols, self.cols, self.rows);
                self.grid = g;
                self.cur.x = nx;
                self.cur.y = ny;
                self.cur.pending = np;
            } else {
                self.grid = p;
            }
        }
        self.in_alt = false;
        self.modes.alt_screen = false;
        self.mark_all();
    }

    pub fn reset(&mut self) {
        self.cur = Cursor { x: 0, y: 0, pen: Style::DEFAULT, pending: false };
        self.saved = None;
        let alt = self.modes.alt_screen;
        self.modes = Modes { alt_screen: alt, ..Modes::default() };
        self.top = 0;
        self.bot = self.rows - 1;
        self.tabs = default_tabs(self.cols);
        self.g = [Charset::Ascii; 2];
        self.gl = 0;
        self.clear_screen();
        self.mark_all();
    }

    fn soft_reset(&mut self) {
        self.modes.insert = false;
        self.modes.origin = false;
        self.modes.autowrap = true;
        self.modes.cursor_visible = true;
        self.modes.app_cursor = false;
        self.modes.app_keypad = false;
        self.top = 0;
        self.bot = self.rows - 1;
        self.cur.pen = Style::DEFAULT;
        self.cur.pending = false;
        self.saved = None;
        self.g = [Charset::Ascii; 2];
        self.gl = 0;
    }

    /// Resize following tmux semantics: vertical resize first (rows below the
    /// cursor are dropped, then rows move to/from history), then the primary
    /// screen is reflowed and the bottom rows become the screen.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.clamp(2, 4096);
        let rows = rows.clamp(1, 4096);
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.end_run();
        let old_cols = self.cols;
        if self.in_alt {
            // tmux resizes the saved primary screen only when leaving the
            // alternate screen; here only the alternate grid changes.
            let tx = self.tx();
            let (g, nx, ny) = alt_resize(std::mem::take(&mut self.grid), tx, self.cur.y, cols, rows);
            self.grid = g;
            self.cur.x = nx.min(cols - 1);
            self.cur.y = ny;
            self.cur.pending = nx >= cols;
        } else {
            let (x, y, p) = (self.cur.x, self.cur.y, self.cur.pending);
            let grid = std::mem::take(&mut self.grid);
            let (g, nx, ny, np) = self.resize_primary(grid, x, y, p, old_cols, cols, rows);
            self.grid = g;
            self.cur.x = nx;
            self.cur.y = ny;
            self.cur.pending = np;
        }
        self.cols = cols;
        self.rows = rows;
        self.top = 0;
        self.bot = rows - 1;
        self.tabs = default_tabs(cols);
        if let Some(s) = self.saved.as_mut() {
            s.x = s.x.min(cols - 1);
            s.y = s.y.min(rows - 1);
        }
        self.dirty = vec![0; rows.div_ceil(64)];
        self.mark_all();
    }

    /// Pull up to `n` physical rows (wrapped at `width`) back from history.
    fn pull_history_rows(&mut self, n: usize, width: usize) -> Vec<Row> {
        let mut out: Vec<Row> = Vec::new();
        while out.len() < n {
            let was_open = self.hist.is_open() && out.is_empty();
            let Some(l) = self.hist.pop_newest() else { break };
            let cells = history::cellize(history::runs(&l), &mut self.clusters);
            let mark = history::line_flags(&l);
            let mut rows: Vec<Row> = history::wrap_cells(&cells, width)
                .into_iter()
                .enumerate()
                .map(|(k, (mut rc, wr))| {
                    let used = rc.len() as u16;
                    rc.resize(width, Cell::EMPTY);
                    Row { cells: rc, wrapped: wr, mark: if k == 0 { mark } else { 0 }, used, size: used }
                })
                .collect();
            if was_open && let Some(r) = rows.last_mut() {
                r.wrapped = true;
            }
            let need = n - out.len();
            if rows.len() > need {
                let keep: Vec<Row> = rows.drain(..rows.len() - need).collect();
                for r in &keep {
                    self.hist.push_row(&r.cells[..r.used as usize], r.wrapped, r.mark, &self.clusters);
                }
                // The kept part continues into the rows pulled back.
            }
            rows.append(&mut out);
            out = rows;
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn resize_primary(
        &mut self,
        mut grid: Vec<Row>,
        cx: usize,
        cy: usize,
        pending: bool,
        old_cols: usize,
        cols: usize,
        rows: usize,
    ) -> (Vec<Row>, usize, usize, bool) {
        let oldy = grid.len();
        let mut cy = cy.min(oldy.saturating_sub(1));
        let mut cx = if pending { old_cols } else { cx };
        // 1. Vertical resize at the old width (tmux screen_resize_y).
        if rows < oldy {
            let mut needed = oldy - rows;
            let available = (oldy - 1 - cy).min(needed);
            if available > 0 {
                grid.truncate(oldy - available);
                if let Some(r) = grid.last_mut() {
                    r.wrapped = false;
                }
            }
            needed -= available;
            if needed > 0 {
                for r in grid.drain(..needed) {
                    r.push_to(&mut self.hist, &self.clusters);
                }
                self.hscrolled += needed;
                cy -= needed;
            }
        } else if rows > oldy {
            let needed = rows - oldy;
            let avail = self.hscrolled.min(needed);
            if avail > 0 {
                let mut pulled = self.pull_history_rows(avail, old_cols);
                let n = pulled.len();
                self.hscrolled -= n;
                pulled.append(&mut grid);
                grid = pulled;
                cy += n;
            }
            while grid.len() < rows {
                grid.push(Row::new(old_cols, Cell::EMPTY));
            }
        }
        if cols == old_cols {
            self.hist.close_if_empty();
            for r in &mut grid {
                r.cells.resize(cols, Cell::EMPTY);
            }
            let pend = cx >= cols;
            return (grid, cx.min(cols - 1), cy.min(rows - 1), pend);
        }
        // 2. Reflow (tmux grid_reflow) over [open history tail] + screen.
        let mut lines: Vec<RLine> = Vec::new();
        if self.hist.is_open()
            && let Some(l) = self.hist.pop_newest()
        {
            let cells = history::cellize(history::runs(&l), &mut self.clusters);
            let mark = history::line_flags(&l);
            for (k, (rc, _)) in history::wrap_cells(&cells, old_cols).into_iter().enumerate() {
                lines.push(RLine { cells: rc, wrapped: true, mark: if k == 0 { mark } else { 0 } });
            }
        }
        let tail = lines.len();
        for r in grid {
            let used = (r.used as usize).min(r.cells.len());
            let mut cells = r.cells;
            cells.truncate(used);
            lines.push(RLine { cells, wrapped: r.wrapped, mark: r.mark });
        }
        // Cursor as (offset in logical line, logical line index) - grid_wrap_position.
        let cyy = tail + cy;
        let (mut ax, mut ay) = (0usize, 0usize);
        for l in &lines[..cyy] {
            if l.wrapped {
                ax += l.cells.len();
            } else {
                ax = 0;
                ay += 1;
            }
        }
        let wx = if cx >= lines[cyy].cells.len() { usize::MAX } else { ax + cx };
        let wy = ay;
        let lines = reflow_lines(lines, cols, &self.clusters);
        // grid_unwrap_position
        let mut yy = 0usize;
        let mut ly = 0usize;
        while yy + 1 < lines.len() {
            if ly == wy {
                break;
            }
            if !lines[yy].wrapped {
                ly += 1;
            }
            yy += 1;
        }
        let mut wx2 = wx;
        if wx2 == usize::MAX {
            while yy + 1 < lines.len() && lines[yy].wrapped {
                yy += 1;
            }
            wx2 = lines[yy].cells.len();
        } else {
            while yy + 1 < lines.len() && lines[yy].wrapped {
                if wx2 < lines[yy].cells.len() {
                    break;
                }
                wx2 -= lines[yy].cells.len();
                yy += 1;
            }
        }
        cx = wx2;
        let mut cyabs = yy as isize;
        // 3. The bottom `rows` lines form the screen.
        let mut out: Vec<Row> = lines
            .into_iter()
            .map(|l| {
                let used = l.cells.len().min(cols) as u16;
                let mut cells = l.cells;
                cells.resize(cols, Cell::EMPTY);
                Row { cells, wrapped: l.wrapped, mark: l.mark, used, size: used }
            })
            .collect();
        if out.len() > rows {
            let k = out.len() - rows;
            for r in out.drain(..k) {
                r.push_to(&mut self.hist, &self.clusters);
            }
            cyabs -= k as isize;
        } else if out.len() < rows {
            let mut pulled = self.pull_history_rows(rows - out.len(), cols);
            cyabs += pulled.len() as isize;
            pulled.append(&mut out);
            out = pulled;
            while out.len() < rows {
                out.push(Row::new(cols, Cell::EMPTY));
            }
        }
        self.hscrolled = self.hscrolled.min(self.hist.len());
        if cyabs < 0 {
            return (out, 0, 0, false);
        }
        let cy = (cyabs as usize).min(rows - 1);
        let pend = cx >= cols;
        (out, cx.min(cols - 1), cy, pend)
    }

    fn sgr(&mut self, params: &vte::Params) {
        let mut pen = self.cur.pen;
        let mut it = params.iter();
        if params.is_empty() {
            self.cur.pen = Style::DEFAULT;
            return;
        }
        while let Some(p) = it.next() {
            let code = p.first().copied().unwrap_or(0);
            match code {
                0 => pen = Style::DEFAULT,
                1 => pen.fg |= BOLD,
                2 => pen.fg |= DIM,
                3 => pen.fg |= ITALIC,
                4 => {
                    let style = if p.len() > 1 { p[1] as u32 } else { 1 };
                    pen.set_underline(style);
                }
                5 | 6 => pen.fg |= BLINK,
                7 => pen.fg |= REVERSE,
                8 => pen.fg |= HIDDEN,
                9 => pen.bg |= STRIKE,
                21 => pen.set_underline(2),
                22 => pen.fg &= !(BOLD | DIM),
                23 => pen.fg &= !ITALIC,
                24 => pen.set_underline(0),
                25 => pen.fg &= !BLINK,
                27 => pen.fg &= !REVERSE,
                28 => pen.fg &= !HIDDEN,
                29 => pen.bg &= !STRIKE,
                30..=37 => pen.set_fg(color_idx((code - 30) as u8)),
                39 => pen.set_fg(0),
                40..=47 => pen.set_bg(color_idx((code - 40) as u8)),
                49 => pen.set_bg(0),
                53 => pen.bg |= OVERLINE,
                55 => pen.bg &= !OVERLINE,
                90..=97 => pen.set_fg(color_idx((code - 90 + 8) as u8)),
                100..=107 => pen.set_bg(color_idx((code - 100 + 8) as u8)),
                38 | 48 | 58 => {
                    let color = if p.len() > 1 {
                        match p[1] {
                            5 if p.len() > 2 => Some(color_idx(p[2] as u8)),
                            2 if p.len() >= 6 => Some(color_rgb(p[3] as u8, p[4] as u8, p[5] as u8)),
                            2 if p.len() == 5 => Some(color_rgb(p[2] as u8, p[3] as u8, p[4] as u8)),
                            _ => None,
                        }
                    } else {
                        match it.next().and_then(|q| q.first().copied()) {
                            Some(5) => it.next().and_then(|q| q.first().copied()).map(|i| color_idx(i as u8)),
                            Some(2) => {
                                let r = it.next().and_then(|q| q.first().copied()).unwrap_or(0);
                                let g = it.next().and_then(|q| q.first().copied()).unwrap_or(0);
                                let b = it.next().and_then(|q| q.first().copied()).unwrap_or(0);
                                Some(color_rgb(r as u8, g as u8, b as u8))
                            }
                            _ => None,
                        }
                    };
                    if let Some(c) = color {
                        match code {
                            38 => pen.set_fg(c),
                            48 => pen.set_bg(c),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        self.cur.pen = pen;
    }

    fn set_private_mode(&mut self, m: u16, on: bool) {
        match m {
            1 => self.modes.app_cursor = on,
            5 => {
                self.modes.reverse = on;
                self.mark_all();
            }
            6 => {
                self.modes.origin = on;
                self.goto(0, 0);
            }
            7 => self.modes.autowrap = on,
            9 | 1000 | 1002 | 1003 => {
                if on {
                    self.modes.mouse = m;
                } else if self.modes.mouse == m || m != 9 {
                    self.modes.mouse = MOUSE_NONE;
                }
            }
            12 => self.modes.cursor_blink = on,
            25 => self.modes.cursor_visible = on,
            47 | 1047 => {
                if on {
                    self.enter_alt(false)
                } else {
                    self.leave_alt(false)
                }
            }
            1049 => {
                if on {
                    self.enter_alt(true)
                } else {
                    self.leave_alt(true)
                }
            }
            1004 => self.modes.focus = on,
            1005 | 1006 | 1015 => {
                if on {
                    self.modes.mouse_enc = m;
                } else if self.modes.mouse_enc == m {
                    self.modes.mouse_enc = 0;
                }
            }
            1007 => self.modes.alt_scroll = on,
            2004 => self.modes.bracketed_paste = on,
            2026 => self.modes.sync = on,
            _ => {}
        }
    }

    fn private_mode_state(&self, m: u16) -> u8 {
        let b = |v: bool| if v { 1 } else { 2 };
        match m {
            1 => b(self.modes.app_cursor),
            5 => b(self.modes.reverse),
            6 => b(self.modes.origin),
            7 => b(self.modes.autowrap),
            9 | 1000 | 1002 | 1003 => b(self.modes.mouse == m),
            12 => b(self.modes.cursor_blink),
            25 => b(self.modes.cursor_visible),
            47 | 1047 | 1049 => b(self.in_alt),
            1004 => b(self.modes.focus),
            1005 | 1006 | 1015 => b(self.modes.mouse_enc == m),
            1007 => b(self.modes.alt_scroll),
            2004 => b(self.modes.bracketed_paste),
            2026 => b(self.modes.sync),
            _ => 0,
        }
    }

    fn respond(&mut self, s: &str) {
        self.out.extend_from_slice(s.as_bytes());
    }

    /// Replay a known screen state (used for crash recovery salvage).
    pub fn screen_text(&self) -> Vec<String> {
        self.grid.iter().map(|r| row_text(r, &self.clusters)).collect()
    }

    /// Plain text of history + screen (oldest first). `join` merges wrapped rows.
    pub fn all_text(&self) -> Vec<String> {
        let mut v: Vec<String> = self.hist.iter().map(history::line_text).collect();
        let grid = if self.in_alt { self.other.as_deref().unwrap_or(&self.grid) } else { &self.grid };
        let mut cont = self.hist.is_open() && !v.is_empty();
        for r in grid {
            let t = row_text(r, &self.clusters);
            if cont {
                v.last_mut().unwrap().push_str(&t);
            } else {
                v.push(t);
            }
            cont = r.wrapped;
        }
        while v.last().is_some_and(|l| l.is_empty()) {
            v.pop();
        }
        v
    }

    /// Snapshot: history + primary screen as encoded lines (for reboot resurrection).
    pub fn snapshot(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut h = History::new(self.hist.limit + self.rows + 1);
        for l in self.hist.iter() {
            h.push_encoded(l);
        }
        let grid = if self.in_alt { self.other.as_deref().unwrap_or(&self.grid) } else { &self.grid };
        let mut last = grid.len();
        while last > 0 && grid[last - 1].cells.iter().all(|c| c.is_default_blank()) {
            last -= 1;
        }
        for r in &grid[..last] {
            r.push_to(&mut h, &self.clusters);
        }
        h.serialize(&mut out);
        out
    }

    pub fn load_snapshot(&mut self, data: &[u8]) {
        let mut h = History::new(self.hist.limit);
        let mut pos = 0;
        if h.deserialize(data, &mut pos).is_some() {
            self.hist = h;
        }
    }
}

pub fn row_text(r: &Row, clusters: &Clusters) -> String {
    let cells = r.line_cells();
    let mut s = String::with_capacity(cells.len());
    for c in cells {
        push_cell_text(&mut s, c, clusters);
    }
    if !r.wrapped {
        let t = s.trim_end_matches(' ').len();
        s.truncate(t);
    }
    s
}

/// tmux screen_resize_y without history plus a width change without reflow.
/// `x` may equal the old width (pending wrap); returns the unclamped x.
fn alt_resize(mut g: Vec<Row>, x: usize, y: usize, cols: usize, rows: usize) -> (Vec<Row>, usize, usize) {
    let oldy = g.len();
    let mut y = y.min(oldy.saturating_sub(1));
    if rows < oldy {
        let mut needed = oldy - rows;
        let available = (oldy - 1 - y).min(needed);
        if available > 0 {
            g.truncate(oldy - available);
        }
        needed -= available;
        let top = needed.min(y);
        if top > 0 {
            g.drain(..top);
            y -= top;
        }
        g.truncate(rows);
    }
    while g.len() < rows {
        g.push(Row::new(cols, Cell::EMPTY));
    }
    for r in &mut g {
        r.cells.resize(cols, Cell::EMPTY);
        r.used = r.used.min(cols as u16);
        r.size = r.size.min(cols as u16);
    }
    (g, x, y.min(rows - 1))
}

struct RLine {
    cells: Vec<Cell>,
    wrapped: bool,
    mark: u8,
}

/// Width-limited prefix length (cells) that fits in `sx` without splitting a wide char.
fn fit_prefix(cells: &[Cell], start_width: usize, sx: usize) -> usize {
    let mut width = start_width;
    let mut i = 0;
    while i < cells.len() {
        let w = if cells[i].ch == WIDE_SPACER {
            0
        } else if cells.get(i + 1).is_some_and(|n| n.ch == WIDE_SPACER) {
            2
        } else {
            1
        };
        if width + w > sx {
            break;
        }
        width += w;
        i += 1;
    }
    i
}

fn cells_width(cells: &[Cell]) -> usize {
    cells.iter().filter(|c| c.ch != WIDE_SPACER).count() + cells.windows(2).filter(|w| w[1].ch == WIDE_SPACER && w[0].ch != WIDE_SPACER).count()
}

/// Port of tmux grid_reflow (greedy row-by-row split/join).
fn reflow_lines(src: Vec<RLine>, sx: usize, _clusters: &Clusters) -> Vec<RLine> {
    let mut src: std::collections::VecDeque<RLine> = src.into();
    let mut out: Vec<RLine> = Vec::with_capacity(src.len());
    while let Some(line) = src.pop_front() {
        let width = cells_width(&line.cells);
        if width == sx {
            out.push(line);
            continue;
        }
        if width > sx {
            let wrapped = line.wrapped;
            let mark = line.mark;
            let mut rest = line.cells;
            let mut first = true;
            let mut last_width;
            loop {
                let n = fit_prefix(&rest, 0, sx).max(1);
                let piece: Vec<Cell> = rest.drain(..n.min(rest.len())).collect();
                last_width = cells_width(&piece);
                let more = !rest.is_empty();
                out.push(RLine { cells: piece, wrapped: more || wrapped, mark: if first { mark } else { 0 } });
                first = false;
                if !more {
                    break;
                }
            }
            if last_width < sx && wrapped {
                reflow_join(&mut out, &mut src, sx, last_width);
            }
            continue;
        }
        let wrapped = line.wrapped;
        out.push(line);
        if wrapped {
            reflow_join(&mut out, &mut src, sx, width);
        }
    }
    out
}

fn reflow_join(out: &mut [RLine], src: &mut std::collections::VecDeque<RLine>, sx: usize, mut width: usize) {
    let Some(target) = out.last_mut() else { return };
    let mut wrapped = true;
    let mut consumed_any = false;
    let mut partial = false;
    while let Some(next) = src.front_mut() {
        if !next.wrapped {
            wrapped = false;
        }
        if next.cells.is_empty() {
            if !wrapped {
                break;
            }
            src.pop_front();
            consumed_any = true;
            continue;
        }
        let want = fit_prefix(&next.cells, width, sx);
        if want == 0 {
            break;
        }
        width += cells_width(&next.cells[..want]);
        target.cells.extend_from_slice(&next.cells[..want]);
        consumed_any = true;
        if want < next.cells.len() {
            next.cells.drain(..want);
            partial = true;
            break;
        }
        src.pop_front();
        if !wrapped || width == sx {
            break;
        }
    }
    if consumed_any && !partial && !wrapped {
        target.wrapped = false;
    }
}

impl vte::Perform for Term {
    #[inline]
    fn print(&mut self, c: char) {
        if (c as u32).wrapping_sub(0x20) < 0x5f {
            self.abuf.push(c as u8);
            if self.abuf.len() >= 4096 {
                self.flush_ascii();
            }
            return;
        }
        self.flush_ascii();
        self.put_char(c);
    }

    fn execute(&mut self, b: u8) {
        self.flush_ascii();
        self.last_char = None;
        self.end_run();
        match b {
            0x07 => self.events.push(Event::Bell),
            0x08 => self.backspace(),
            0x09 => self.tab(1),
            0x0a..=0x0c => {
                self.linefeed();
                if self.modes.lnm {
                    self.cr();
                }
            }
            0x0d => self.cr(),
            0x0e => self.gl = 1,
            0x0f => self.gl = 0,
            0x84 => self.linefeed(),
            0x85 => {
                self.cr();
                self.linefeed();
            }
            0x88 => {
                if !self.cur.pending {
                    let x = self.cur.x;
                    self.tabs[x] = true;
                }
            }
            0x8d => self.reverse_index(),
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &vte::Params, inter: &[u8], ignore: bool, action: char) {
        self.flush_ascii();
        self.end_run();
        if ignore {
            return;
        }
        let private = inter.first().copied();
        match (private, action) {
            (None, '@') => self.insert_cells(arg(params, 0, 1)),
            (None, 'A') => self.move_up(arg(params, 0, 1)),
            (None, 'B' | 'e') => self.move_down(arg(params, 0, 1)),
            (None, 'C' | 'a') => {
                // From a pending wrap tmux's set_cursor clamps to the last column.
                self.cur.x = (self.cur.x + arg(params, 0, 1)).min(self.cols - 1);
                self.cur.pending = false;
            }
            (None, 'D') => {
                let tx = self.tx();
                let n = arg(params, 0, 1).min(tx);
                if n > 0 {
                    self.cur.x = (tx - n).min(self.cols - 1);
                    self.cur.pending = false;
                }
            }
            (None, 'E') => {
                self.cr();
                self.move_down(arg(params, 0, 1));
            }
            (None, 'F') => {
                self.cr();
                self.move_up(arg(params, 0, 1));
            }
            (None, 'G' | '`') => {
                self.cur.x = (arg(params, 0, 1) - 1).min(self.cols - 1);
                self.cur.pending = false;
            }
            (None, 'H' | 'f') => {
                let y = arg(params, 0, 1) - 1;
                let x = arg(params, 1, 1) - 1;
                self.goto(x, y);
            }
            (None, 'I') => self.tab(arg(params, 0, 1)),
            (None, 'J') => match arg0(params, 0).unwrap_or(0) {
                0 => {
                    let (x, y) = (self.cur.x, self.cur.y);
                    if x == 0 && y == 0 && !self.cur.pending && !self.in_alt {
                        self.clear_screen();
                    } else {
                        if !self.cur.pending {
                            self.erase_cells(y, x, self.cols);
                        }
                        self.erase_rows(y + 1, self.rows);
                    }
                    self.mark_all();
                }
                1 => {
                    let y = self.cur.y;
                    self.erase_rows(0, y);
                    let end = if self.cur.pending { self.cols } else { self.cur.x + 1 };
                    self.erase_cells(y, 0, end);
                }
                2 => self.clear_screen(),
                3 if arg0(params, 1).unwrap_or(0) == 0 => {
                    self.hist.clear();
                    self.hscrolled = 0;
                }
                _ => {}
            },
            (None, 'K') => {
                let y = self.cur.y;
                match arg0(params, 0).unwrap_or(0) {
                    0 => {
                        if !self.cur.pending {
                            let x = self.cur.x;
                            self.erase_cells(y, x, self.cols);
                        }
                    }
                    1 => {
                        let end = if self.cur.pending || self.cur.x >= self.cols - 1 { self.cols } else { self.cur.x + 1 };
                        self.erase_cells(y, 0, end);
                    }
                    2 => self.erase_cells(y, 0, self.cols),
                    _ => {}
                }
            }
            (None, 'L') => self.insert_lines(self.cur.y, arg(params, 0, 1)),
            (None, 'M') => self.delete_lines(self.cur.y, arg(params, 0, 1)),
            (None, 'P') => self.delete_cells(arg(params, 0, 1)),
            (None, 'S') => self.scroll_up(arg(params, 0, 1)),
            (None, 'T') if params.len() <= 1 => self.scroll_down(arg(params, 0, 1)),
            (None, 'X') => {
                if !self.cur.pending {
                    let (x, y) = (self.cur.x, self.cur.y);
                    let n = arg(params, 0, 1);
                    self.erase_cells(y, x, x + n);
                }
            }
            (None, 'Z') => self.back_tab(arg(params, 0, 1)),
            (None, 'b') => {
                if let Some(c) = self.last_char {
                    let n = arg(params, 0, 1).min(self.cols - self.tx());
                    for _ in 0..n {
                        self.put_char(c);
                    }
                }
            }
            (None, 'c') => {
                if arg0(params, 0).unwrap_or(0) == 0 {
                    self.respond("\x1b[?1;2c");
                }
            }
            (Some(b'>'), 'c') => {
                if arg0(params, 0).unwrap_or(0) == 0 {
                    self.respond("\x1b[>84;0;0c");
                }
            }
            (None, 'd') => {
                let y = arg(params, 0, 1) - 1;
                let (ymin, ymax) = if self.modes.origin { (self.top, self.bot) } else { (0, self.rows - 1) };
                self.cur.y = (y + if self.modes.origin { self.top } else { 0 }).clamp(ymin, ymax);
            }
            (None, 'g') => match arg0(params, 0).unwrap_or(0) {
                0 => {
                    if !self.cur.pending {
                        let x = self.cur.x;
                        self.tabs[x] = false;
                    }
                }
                3 => self.tabs.iter_mut().for_each(|t| *t = false),
                _ => {}
            },
            (None, 'h' | 'l') => {
                let on = action == 'h';
                for p in params.iter() {
                    match p.first().copied().unwrap_or(0) {
                        4 => self.modes.insert = on,
                        20 => self.modes.lnm = on,
                        _ => {}
                    }
                }
            }
            (Some(b'?'), 'h' | 'l') => {
                let on = action == 'h';
                for p in params.iter() {
                    self.set_private_mode(p.first().copied().unwrap_or(0), on);
                }
            }
            (None, 'm') => self.sgr(params),
            (None, 'n') => match arg0(params, 0).unwrap_or(0) {
                5 => self.respond("\x1b[0n"),
                6 => {
                    let s = format!("\x1b[{};{}R", self.cur.y + 1, self.tx() + 1);
                    self.respond(&s);
                }
                _ => {}
            },
            (Some(b'?'), 'n') => {
                if arg0(params, 0).unwrap_or(0) == 6 {
                    let s = format!("\x1b[?{};{}R", self.cur.y + 1, self.tx() + 1);
                    self.respond(&s);
                }
            }
            (Some(b'!'), 'p') => self.soft_reset(),
            (Some(b'?'), 'p') if inter.get(1) == Some(&b'$') => {
                let m = arg0(params, 0).unwrap_or(0);
                let st = self.private_mode_state(m);
                let s = format!("\x1b[?{};{}$y", m, st);
                self.respond(&s);
            }
            (Some(b'$'), 'p') => {
                let m = arg0(params, 0).unwrap_or(0);
                let st = match m {
                    4 => 2 - self.modes.insert as u8,
                    20 => 2 - self.modes.lnm as u8,
                    _ => 0,
                };
                let s = format!("\x1b[{};{}$y", m, st);
                self.respond(&s);
            }
            (Some(b' '), 'q') => {
                self.modes.cursor_shape = arg0(params, 0).unwrap_or(0).min(6) as u8;
            }
            (Some(b'>'), 'q') => {
                if arg0(params, 0).unwrap_or(0) == 0 {
                    self.respond(concat!("\x1bP>|amux ", env!("CARGO_PKG_VERSION"), "\x1b\\"))
                }
            }
            (None, 'r') => {
                let top = arg(params, 0, 1) - 1;
                let bot = arg(params, 1, self.rows).min(self.rows) - 1;
                let top = top.min(self.rows - 1);
                if top < bot {
                    self.top = top;
                    self.bot = bot;
                    self.cur.x = 0;
                    self.cur.y = 0;
                    self.cur.pending = false;
                }
            }
            (None, 's') => self.save_cursor(),
            (None, 'u') => self.restore_cursor(),
            (None, 't') => match arg0(params, 0).unwrap_or(0) {
                14 => {
                    let s = format!("\x1b[4;{};{}t", self.rows * 16, self.cols * 8);
                    self.respond(&s);
                }
                16 => self.respond("\x1b[6;16;8t"),
                18 => {
                    let s = format!("\x1b[8;{};{}t", self.rows, self.cols);
                    self.respond(&s);
                }
                22 => {
                    if self.title_stack.len() < 16 {
                        self.title_stack.push(self.title.clone());
                    }
                }
                23 => {
                    if let Some(t) = self.title_stack.pop() {
                        self.title = t.clone();
                        self.events.push(Event::Title(t));
                    }
                }
                _ => {}
            },
            _ => {}
        }
        self.last_char = None;
    }

    fn esc_dispatch(&mut self, inter: &[u8], ignore: bool, byte: u8) {
        self.flush_ascii();
        self.end_run();
        if ignore {
            return;
        }
        self.last_char = None;
        match (inter.first().copied(), byte) {
            (None, b'7') => self.save_cursor(),
            (None, b'8') => self.restore_cursor(),
            (None, b'D') => self.linefeed(),
            (None, b'E') => {
                self.cr();
                self.linefeed();
            }
            (None, b'H') => {
                if !self.cur.pending {
                    let x = self.cur.x;
                    self.tabs[x] = true;
                }
            }
            (None, b'M') => self.reverse_index(),
            (None, b'c') => self.reset(),
            (None, b'=') => self.modes.app_keypad = true,
            (None, b'>') => self.modes.app_keypad = false,
            (Some(b'('), b'0') => self.g[0] = Charset::Dec,
            (Some(b'('), b'B') => self.g[0] = Charset::Ascii,
            (Some(b')'), b'0') => self.g[1] = Charset::Dec,
            (Some(b')'), b'B') => self.g[1] = Charset::Ascii,
            (Some(b'#'), b'8') => {
                let cols = self.cols;
                for y in 0..self.rows {
                    self.grid[y].cells[..cols].fill(Cell { ch: 'E' as u32, fg: 0, bg: 0 });
                    self.grid[y].touch(cols);
                    self.mark_dirty(y);
                }
                self.cur.x = 0;
                self.cur.y = 0;
                self.cur.pending = false;
                self.top = 0;
                self.bot = self.rows - 1;
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        self.flush_ascii();
        self.end_run();
        let Some(&first) = params.first() else { return };
        let st = if bell_terminated { "\x07" } else { "\x1b\\" };
        let join = |from: usize| -> String {
            let mut s = Vec::new();
            for (i, p) in params.iter().enumerate().skip(from) {
                if i > from {
                    s.push(b';');
                }
                s.extend_from_slice(p);
            }
            let mut s = String::from_utf8_lossy(&s).into_owned();
            if s.len() > 1024 {
                let mut cut = 1024;
                while !s.is_char_boundary(cut) {
                    cut -= 1;
                }
                s.truncate(cut);
            }
            s
        };
        match first {
            b"0" | b"2" => {
                let t: String = join(1).chars().filter(|c| !c.is_control()).take(256).collect();
                if t != self.title {
                    self.title = t.clone();
                    self.events.push(Event::Title(t));
                }
            }
            b"7" => {
                let u = join(1);
                let path = u.strip_prefix("file://").map(|r| r.find('/').map(|i| &r[i..]).unwrap_or(r)).unwrap_or(&u);
                let p = percent_decode(path);
                if !p.is_empty() {
                    self.cwd = Some(p.clone());
                    self.events.push(Event::Cwd(p));
                }
            }
            b"9" => {
                if params.get(1) == Some(&&b"4"[..]) {
                    let num = |i: usize| -> u8 {
                        params.get(i).and_then(|p| std::str::from_utf8(p).ok()).and_then(|s| s.parse::<u32>().ok()).unwrap_or(0).min(255) as u8
                    };
                    self.events.push(Event::Progress(num(2), num(3).min(100)));
                } else {
                    self.events.push(Event::Notify(String::new(), join(1)));
                }
            }
            b"10" | b"11" | b"12" => {
                if params.get(1) == Some(&&b"?"[..])
                    && let Some((fg, bg)) = self.default_colors
                {
                    let (r, g, b) = if first == b"11" { bg } else { fg };
                    let s = format!("\x1b]{};rgb:{:02x}{:02x}/{:02x}{:02x}/{:02x}{:02x}{}", std::str::from_utf8(first).unwrap_or("10"), r, r, g, g, b, b, st);
                    self.respond(&s);
                }
            }
            b"52" => {
                if let Some(data) = params.get(2)
                    && *data != b"?"
                {
                    self.events.push(Event::Clipboard(String::from_utf8_lossy(data).into_owned()));
                }
            }
            b"133" => {
                if let Some(k) = params.get(1).and_then(|p| p.first()).copied() {
                    let y = self.cur.y;
                    if k == b'A' {
                        self.grid[y].mark |= history::LINE_PROMPT;
                    }
                    let code = if k == b'D' { params.get(2).and_then(|p| std::str::from_utf8(p).ok()).and_then(|s| s.parse().ok()) } else { None };
                    self.events.push(Event::Prompt(k, code));
                }
            }
            b"777" if params.get(1) == Some(&&b"notify"[..]) => {
                let t = params.get(2).map(|p| String::from_utf8_lossy(p).into_owned()).unwrap_or_default();
                self.events.push(Event::Notify(t, join(3)));
            }
            _ => {}
        }
    }

    fn hook(&mut self, _params: &vte::Params, inter: &[u8], ignore: bool, action: char) {
        self.flush_ascii();
        self.end_run();
        self.dcs = Dcs::default();
        if ignore {
            return;
        }
        let kind = match (inter.first().copied(), action) {
            (Some(b'$'), 'q') => b'q',
            (Some(b'+'), 'q') => b'+',
            _ => 0,
        };
        self.dcs.active = kind != 0;
        self.dcs.kind = kind;
    }

    fn put(&mut self, byte: u8) {
        self.flush_ascii();
        if self.dcs.active && self.dcs.buf.len() < 256 {
            self.dcs.buf.push(byte);
        }
    }

    fn unhook(&mut self) {
        self.flush_ascii();
        if !self.dcs.active {
            return;
        }
        let buf = std::mem::take(&mut self.dcs.buf);
        match self.dcs.kind {
            b'q' => {
                let reply = match &buf[..] {
                    b" q" => Some(format!("{} q", self.modes.cursor_shape.max(1))),
                    b"m" => Some("0m".to_string()),
                    b"r" => Some(format!("{};{}r", self.top + 1, self.bot + 1)),
                    _ => None,
                };
                match reply {
                    Some(r) => {
                        let s = format!("\x1bP1$r{}\x1b\\", r);
                        self.respond(&s);
                    }
                    None => self.respond("\x1bP0$r\x1b\\"),
                }
            }
            b'+' => self.respond("\x1bP0+r\x1b\\"),
            _ => {}
        }
        self.dcs = Dcs::default();
    }
}

pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = ((b[i + 1] as char).to_digit(16), (b[i + 2] as char).to_digit(16))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Convenience wrapper bundling a parser with a terminal.
pub struct Emulator {
    pub parser: vte::Parser,
    pub term: Term,
}

impl Emulator {
    pub fn new(cols: usize, rows: usize, history: usize) -> Emulator {
        Emulator { parser: vte::Parser::new(), term: Term::new(cols, rows, history) }
    }
    pub fn feed(&mut self, b: &[u8]) {
        self.term.feed(&mut self.parser, b);
    }
}
