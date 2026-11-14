//! Keys: parsing terminal input into key events, tmux key names, and
//! encoding keys back into bytes for a pane.

use crate::term::Modes;

pub const CTRL: u8 = 1;
pub const META: u8 = 2;
pub const SHIFT: u8 = 4;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Code {
    Char(char),
    F(u8),
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    Enter,
    Tab,
    BTab,
    Backspace,
    Escape,
    /// Mouse binding names (MouseDown1Pane etc.) as an interned index.
    Mouse(u16),
    FocusIn,
    FocusOut,
    Any,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Key {
    pub code: Code,
    pub mods: u8,
}

impl Key {
    pub fn new(code: Code, mods: u8) -> Key {
        Key { code, mods }.normalize()
    }
    pub fn ch(c: char) -> Key {
        Key::new(Code::Char(c), 0)
    }
    pub fn is_nav_down(self) -> bool {
        matches!(self.code, Code::Down) || self == Key::ch('j') || self == Key::new(Code::Char('n'), CTRL)
    }
    pub fn is_nav_up(self) -> bool {
        matches!(self.code, Code::Up) || self == Key::ch('k') || self == Key::new(Code::Char('p'), CTRL)
    }
    pub fn is_quit(self) -> bool {
        matches!(self.code, Code::Escape) || self == Key::ch('q') || self == Key::new(Code::Char('c'), CTRL)
    }
    /// Scroll `cur` inside `0..=end` by line, page (`page`), or home/end (`g`/`G`).
    pub fn scroll(self, cur: usize, page: usize, end: usize) -> Option<usize> {
        Some(if self.is_nav_down() {
            (cur + 1).min(end)
        } else if self.is_nav_up() {
            cur.saturating_sub(1)
        } else if matches!(self.code, Code::PageDown) || self == Key::ch(' ') {
            (cur + page).min(end)
        } else if matches!(self.code, Code::PageUp) {
            cur.saturating_sub(page)
        } else if self == Key::ch('g') {
            0
        } else if self == Key::ch('G') {
            end
        } else {
            return None;
        })
    }
    fn normalize(mut self) -> Key {
        if self.mods & CTRL != 0
            && let Code::Char(c) = self.code
        {
            match c {
                'i' | 'I' => {
                    self.code = Code::Tab;
                    self.mods &= !CTRL;
                }
                'm' | 'M' => {
                    self.code = Code::Enter;
                    self.mods &= !CTRL;
                }
                '[' => {
                    self.code = Code::Escape;
                    self.mods &= !CTRL;
                }
                'A'..='Z' => self.code = Code::Char(c.to_ascii_lowercase()),
                '@' => self.code = Code::Char(' '),
                _ => {}
            }
        }
        self
    }
}

/// Mouse binding names we recognise, indexed by `Code::Mouse`.
pub const MOUSE_NAMES: &[&str] = &[
    "MouseDown1Pane",
    "MouseDown2Pane",
    "MouseDown3Pane",
    "MouseUp1Pane",
    "MouseDrag1Pane",
    "MouseDragEnd1Pane",
    "WheelUpPane",
    "WheelDownPane",
    "MouseDown1Status",
    "MouseDrag1Border",
    "DoubleClick1Pane",
    "TripleClick1Pane",
    "MouseDown3Status",
    "WheelUpStatus",
    "WheelDownStatus",
];

pub fn parse_key_name(s: &str) -> Option<Key> {
    let mut mods = 0u8;
    let mut rest = s;
    loop {
        if rest.len() > 2 && (rest.starts_with("C-") || rest.starts_with("c-")) {
            mods |= CTRL;
            rest = &rest[2..];
        } else if rest.len() > 2 && (rest.starts_with("M-") || rest.starts_with("m-")) {
            mods |= META;
            rest = &rest[2..];
        } else if rest.len() > 2 && (rest.starts_with("S-") || rest.starts_with("s-")) {
            mods |= SHIFT;
            rest = &rest[2..];
        } else if rest.len() == 2 && rest.starts_with('^') {
            mods |= CTRL;
            rest = &rest[1..];
        } else {
            break;
        }
    }
    let lower = rest.to_ascii_lowercase();
    let code = match lower.as_str() {
        "up" => Code::Up,
        "down" => Code::Down,
        "left" => Code::Left,
        "right" => Code::Right,
        "home" => Code::Home,
        "end" => Code::End,
        "npage" | "pagedown" | "pgdn" => Code::PageDown,
        "ppage" | "pageup" | "pgup" => Code::PageUp,
        "ic" | "insert" => Code::Insert,
        "dc" | "delete" => Code::Delete,
        "enter" => Code::Enter,
        "tab" => Code::Tab,
        "btab" => Code::BTab,
        "bspace" => Code::Backspace,
        "escape" => Code::Escape,
        "space" => Code::Char(' '),
        "any" => Code::Any,
        "focusin" => Code::FocusIn,
        "focusout" => Code::FocusOut,
        _ => {
            if let Some(i) = MOUSE_NAMES.iter().position(|m| m.eq_ignore_ascii_case(rest)) {
                Code::Mouse(i as u16)
            } else if lower.len() >= 2
                && lower.starts_with('f')
                && let Ok(n) = lower[1..].parse::<u8>()
                && (1..=24).contains(&n)
            {
                Code::F(n)
            } else {
                let mut chars = rest.chars();
                let c = chars.next()?;
                if chars.next().is_some() {
                    return None;
                }
                Code::Char(c)
            }
        }
    };
    Some(Key::new(code, mods))
}

pub fn key_name(k: Key) -> String {
    let mut s = String::new();
    if k.mods & CTRL != 0 {
        s.push_str("C-");
    }
    if k.mods & META != 0 {
        s.push_str("M-");
    }
    if k.mods & SHIFT != 0 {
        s.push_str("S-");
    }
    let base = match k.code {
        Code::Char(' ') => "Space".to_string(),
        Code::Char(c) => c.to_string(),
        Code::F(n) => format!("F{}", n),
        Code::Up => "Up".into(),
        Code::Down => "Down".into(),
        Code::Left => "Left".into(),
        Code::Right => "Right".into(),
        Code::Home => "Home".into(),
        Code::End => "End".into(),
        Code::PageUp => "PPage".into(),
        Code::PageDown => "NPage".into(),
        Code::Insert => "IC".into(),
        Code::Delete => "DC".into(),
        Code::Enter => "Enter".into(),
        Code::Tab => "Tab".into(),
        Code::BTab => "BTab".into(),
        Code::Backspace => "BSpace".into(),
        Code::Escape => "Escape".into(),
        Code::Mouse(i) => MOUSE_NAMES.get(i as usize).unwrap_or(&"Mouse").to_string(),
        Code::FocusIn => "FocusIn".into(),
        Code::FocusOut => "FocusOut".into(),
        Code::Any => "Any".into(),
    };
    s.push_str(&base);
    s
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MouseEv {
    /// SGR button code (bits: 0-1 button, 4 shift, 8 meta, 16 ctrl, 32 motion, 64 wheel).
    pub b: u16,
    pub x: u16,
    pub y: u16,
    pub release: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ev {
    Key(Key, Vec<u8>),
    Paste(Vec<u8>),
    Mouse(MouseEv),
    /// Bytes we could not classify; forwarded verbatim.
    Raw(Vec<u8>),
}

/// Incremental parser for bytes from a client terminal.
#[derive(Default)]
pub struct Parser {
    buf: Vec<u8>,
    paste: Option<Vec<u8>>,
}

fn csi_key(params: &[u16], fin: u8) -> Option<Key> {
    let mods = params.get(1).map(|&m| m.saturating_sub(1) as u8).unwrap_or(0);
    let m = (if mods & 1 != 0 { SHIFT } else { 0 }) | (if mods & 2 != 0 { META } else { 0 }) | (if mods & 4 != 0 { CTRL } else { 0 });
    let code = match fin {
        b'A' => Code::Up,
        b'B' => Code::Down,
        b'C' => Code::Right,
        b'D' => Code::Left,
        b'H' => Code::Home,
        b'F' => Code::End,
        b'P' => Code::F(1),
        b'Q' => Code::F(2),
        b'R' => Code::F(3),
        b'S' => Code::F(4),
        b'Z' => return Some(Key::new(Code::BTab, 0)),
        b'~' => match params.first().copied().unwrap_or(0) {
            1 | 7 => Code::Home,
            2 => Code::Insert,
            3 => Code::Delete,
            4 | 8 => Code::End,
            5 => Code::PageUp,
            6 => Code::PageDown,
            11..=15 => Code::F((params[0] - 10) as u8),
            17..=21 => Code::F((params[0] - 11) as u8),
            23..=26 => Code::F((params[0] - 12) as u8),
            28 | 29 => Code::F((params[0] - 13) as u8),
            31..=34 => Code::F((params[0] - 14) as u8),
            _ => return None,
        },
        b'u' => {
            // CSI codepoint ; mods u (fixterms / kitty basic)
            let cp = params.first().copied().unwrap_or(0) as u32;
            let c = char::from_u32(cp)?;
            return Some(Key::new(
                match cp {
                    13 => Code::Enter,
                    9 => Code::Tab,
                    27 => Code::Escape,
                    127 => Code::Backspace,
                    _ => Code::Char(c),
                },
                m,
            ));
        }
        _ => return None,
    };
    Some(Key::new(code, m))
}

impl Parser {
    pub fn has_pending(&self) -> bool {
        !self.buf.is_empty()
    }

    pub fn in_paste(&self) -> bool {
        self.paste.is_some()
    }

    /// Flush a pending lone ESC (escape-time expired).
    pub fn timeout(&mut self) -> Vec<Ev> {
        let mut out = Vec::new();
        if self.paste.is_some() {
            return out;
        }
        if !self.buf.is_empty() {
            let b = std::mem::take(&mut self.buf);
            if b == [0x1b] {
                out.push(Ev::Key(Key::new(Code::Escape, 0), b));
            } else if b.len() == 2 && b[0] == 0x1b {
                // ESC + byte: meta key
                let mut p = Parser::default();
                for e in p.feed(&b[1..]) {
                    if let Ev::Key(k, raw) = e {
                        let mut r = vec![0x1b];
                        r.extend(raw);
                        out.push(Ev::Key(Key::new(k.code, k.mods | META), r));
                    }
                }
            } else {
                out.push(Ev::Raw(b));
            }
        }
        out
    }

    pub fn feed(&mut self, data: &[u8]) -> Vec<Ev> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        let mut i = 0;
        let b = std::mem::take(&mut self.buf);
        while i < b.len() {
            if let Some(p) = self.paste.as_mut() {
                // look for ESC [ 2 0 1 ~
                if let Some(pos) = find(&b[i..], b"\x1b[201~") {
                    p.extend_from_slice(&b[i..i + pos]);
                    out.push(Ev::Paste(self.paste.take().unwrap()));
                    i += pos + 6;
                    continue;
                }
                // keep a possible partial terminator in the buffer
                let keep = partial_suffix(&b[i..], b"\x1b[201~");
                p.extend_from_slice(&b[i..b.len() - keep]);
                self.buf = b[b.len() - keep..].to_vec();
                return out;
            }
            let c = b[i];
            if c == 0x1b {
                match parse_escape(&b[i..]) {
                    Esc::Incomplete => {
                        self.buf = b[i..].to_vec();
                        return out;
                    }
                    Esc::PasteStart(n) => {
                        self.paste = Some(Vec::new());
                        i += n;
                    }
                    Esc::Done(ev, n) => {
                        out.push(ev);
                        i += n;
                    }
                }
                continue;
            }
            if c < 0x20 || c == 0x7f {
                let k = match c {
                    0x00 => Key::new(Code::Char(' '), CTRL),
                    0x09 => Key::new(Code::Tab, 0),
                    0x0d => Key::new(Code::Enter, 0),
                    0x7f => Key::new(Code::Backspace, 0),
                    0x1c => Key::new(Code::Char('\\'), CTRL),
                    0x1d => Key::new(Code::Char(']'), CTRL),
                    0x1e => Key::new(Code::Char('^'), CTRL),
                    0x1f => Key::new(Code::Char('_'), CTRL),
                    _ => Key::new(Code::Char((c + 0x60) as char), CTRL),
                };
                out.push(Ev::Key(k, vec![c]));
                i += 1;
                continue;
            }
            // UTF-8 character
            let len = utf8_len(c);
            if i + len > b.len() {
                self.buf = b[i..].to_vec();
                return out;
            }
            match std::str::from_utf8(&b[i..i + len]) {
                Ok(s) => {
                    let ch = s.chars().next().unwrap();
                    out.push(Ev::Key(Key::ch(ch), b[i..i + len].to_vec()));
                }
                Err(_) => out.push(Ev::Raw(b[i..i + len].to_vec())),
            }
            i += len;
        }
        out
    }
}

fn utf8_len(c: u8) -> usize {
    match c {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

fn partial_suffix(h: &[u8], n: &[u8]) -> usize {
    for k in (1..n.len()).rev() {
        if h.len() >= k && h[h.len() - k..] == n[..k] {
            return k;
        }
    }
    0
}

enum Esc {
    Incomplete,
    PasteStart(usize),
    Done(Ev, usize),
}

fn parse_escape(b: &[u8]) -> Esc {
    if b.len() < 2 {
        return Esc::Incomplete;
    }
    match b[1] {
        b'[' => {
            // CSI
            let mut j = 2;
            if b.get(2) == Some(&b'<') {
                // SGR mouse: ESC [ < b ; x ; y (M|m)
                j = 3;
                while j < b.len() && (b[j].is_ascii_digit() || b[j] == b';') {
                    j += 1;
                }
                if j >= b.len() {
                    return if j - 3 > 32 { Esc::Done(Ev::Raw(b[..j].to_vec()), j) } else { Esc::Incomplete };
                }
                let fin = b[j];
                let nums: Vec<u16> = std::str::from_utf8(&b[3..j]).unwrap_or("").split(';').map(|s| s.parse().unwrap_or(0)).collect();
                if (fin == b'M' || fin == b'm') && nums.len() == 3 {
                    return Esc::Done(
                        Ev::Mouse(MouseEv { b: nums[0], x: nums[1].saturating_sub(1), y: nums[2].saturating_sub(1), release: fin == b'm' }),
                        j + 1,
                    );
                }
                return Esc::Done(Ev::Raw(b[..=j].to_vec()), j + 1);
            }
            if b.get(2) == Some(&b'M') {
                // X10 mouse: ESC [ M b x y
                if b.len() < 6 {
                    return Esc::Incomplete;
                }
                let bt = b[3].wrapping_sub(32) as u16;
                let x = b[4].wrapping_sub(33) as u16;
                let y = b[5].wrapping_sub(33) as u16;
                let release = bt & 3 == 3 && bt & 64 == 0;
                return Esc::Done(Ev::Mouse(MouseEv { b: if release { 0 } else { bt }, x, y, release }), 6);
            }
            while j < b.len() && (0x30..=0x3f).contains(&b[j]) {
                j += 1;
            }
            while j < b.len() && (0x20..=0x2f).contains(&b[j]) {
                j += 1;
            }
            if j >= b.len() {
                return if j > 64 { Esc::Done(Ev::Raw(b.to_vec()), b.len()) } else { Esc::Incomplete };
            }
            let fin = b[j];
            let raw = b[..=j].to_vec();
            let body = std::str::from_utf8(&b[2..j]).unwrap_or("");
            if body == "200" && fin == b'~' {
                return Esc::PasteStart(j + 1);
            }
            if body.is_empty() && fin == b'I' {
                return Esc::Done(Ev::Key(Key::new(Code::FocusIn, 0), raw), j + 1);
            }
            if body.is_empty() && fin == b'O' {
                return Esc::Done(Ev::Key(Key::new(Code::FocusOut, 0), raw), j + 1);
            }
            let params: Vec<u16> = body.split(';').map(|s| s.split(':').next().unwrap_or("").parse().unwrap_or(0)).collect();
            match csi_key(&params, fin) {
                Some(k) => Esc::Done(Ev::Key(k, raw), j + 1),
                None => Esc::Done(Ev::Raw(raw), j + 1),
            }
        }
        b'O' => {
            if b.len() < 3 {
                return Esc::Incomplete;
            }
            let raw = b[..3].to_vec();
            let code = match b[2] {
                b'A' => Some(Code::Up),
                b'B' => Some(Code::Down),
                b'C' => Some(Code::Right),
                b'D' => Some(Code::Left),
                b'H' => Some(Code::Home),
                b'F' => Some(Code::End),
                b'P' => Some(Code::F(1)),
                b'Q' => Some(Code::F(2)),
                b'R' => Some(Code::F(3)),
                b'S' => Some(Code::F(4)),
                b'M' => Some(Code::Enter),
                _ => None,
            };
            match code {
                Some(c) => Esc::Done(Ev::Key(Key::new(c, 0), raw), 3),
                None => Esc::Done(Ev::Raw(raw), 3),
            }
        }
        0x1b => {
            // ESC ESC ... -> meta + escape sequence or Escape key
            match parse_escape(&b[1..]) {
                Esc::Incomplete => Esc::Incomplete,
                Esc::PasteStart(n) => Esc::Done(Ev::Key(Key::new(Code::Escape, 0), vec![0x1b]), 1.min(n)),
                Esc::Done(Ev::Key(k, raw), n) => {
                    let mut r = vec![0x1b];
                    r.extend(raw);
                    Esc::Done(Ev::Key(Key::new(k.code, k.mods | META), r), n + 1)
                }
                Esc::Done(_, _) => Esc::Done(Ev::Key(Key::new(Code::Escape, 0), vec![0x1b]), 1),
            }
        }
        c => {
            // Meta + key
            let mut p = Parser::default();
            let evs = p.feed(&b[1..1 + utf8_len(c).min(b.len() - 1)]);
            if p.has_pending() {
                return Esc::Incomplete;
            }
            match evs.into_iter().next() {
                Some(Ev::Key(k, raw)) => {
                    let mut r = vec![0x1b];
                    r.extend_from_slice(&raw);
                    let n = 1 + raw.len();
                    Esc::Done(Ev::Key(Key::new(k.code, k.mods | META), r), n)
                }
                _ => Esc::Done(Ev::Key(Key::new(Code::Escape, 0), vec![0x1b]), 1),
            }
        }
    }
}

/// Encode a key for a pane (respecting its cursor-key mode).
pub fn encode(k: Key, modes: &Modes) -> Vec<u8> {
    let mut out = Vec::new();
    let xm = 1 + (if k.mods & SHIFT != 0 { 1 } else { 0 }) + (if k.mods & META != 0 { 2 } else { 0 }) + (if k.mods & CTRL != 0 { 4 } else { 0 });
    let cursor = |c: u8, out: &mut Vec<u8>| {
        if xm > 1 {
            out.extend_from_slice(format!("\x1b[1;{}{}", xm, c as char).as_bytes());
        } else if modes.app_cursor {
            out.extend_from_slice(&[0x1b, b'O', c]);
        } else {
            out.extend_from_slice(&[0x1b, b'[', c]);
        }
    };
    let tilde = |n: u8, out: &mut Vec<u8>| {
        if xm > 1 {
            out.extend_from_slice(format!("\x1b[{};{}~", n, xm).as_bytes());
        } else {
            out.extend_from_slice(format!("\x1b[{}~", n).as_bytes());
        }
    };
    match k.code {
        Code::Char(c) => {
            if k.mods & META != 0 {
                out.push(0x1b);
            }
            if k.mods & CTRL != 0 {
                let b = match c {
                    ' ' | '@' | '2' => 0,
                    'a'..='z' => c as u8 - b'a' + 1,
                    '[' | '3' => 0x1b,
                    '\\' | '4' => 0x1c,
                    ']' | '5' => 0x1d,
                    '^' | '6' | '~' => 0x1e,
                    '_' | '7' | '/' | '-' => 0x1f,
                    '8' | '?' => 0x7f,
                    _ => {
                        let mut buf = [0u8; 4];
                        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                        return out;
                    }
                };
                out.push(b);
            } else {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
        Code::Enter => {
            if k.mods & META != 0 {
                out.push(0x1b);
            }
            out.push(b'\r');
        }
        Code::Tab => {
            if k.mods & META != 0 {
                out.push(0x1b);
            }
            out.push(b'\t');
        }
        Code::BTab => out.extend_from_slice(b"\x1b[Z"),
        Code::Backspace => {
            if k.mods & META != 0 {
                out.push(0x1b);
            }
            out.push(0x7f);
        }
        Code::Escape => {
            if k.mods & META != 0 {
                out.push(0x1b);
            }
            out.push(0x1b);
        }
        Code::Up => cursor(b'A', &mut out),
        Code::Down => cursor(b'B', &mut out),
        Code::Right => cursor(b'C', &mut out),
        Code::Left => cursor(b'D', &mut out),
        Code::Home => cursor(b'H', &mut out),
        Code::End => cursor(b'F', &mut out),
        Code::Insert => tilde(2, &mut out),
        Code::Delete => tilde(3, &mut out),
        Code::PageUp => tilde(5, &mut out),
        Code::PageDown => tilde(6, &mut out),
        Code::F(n) => match n {
            1..=4 if xm == 1 => out.extend_from_slice(&[0x1b, b'O', b'P' + n - 1]),
            1..=4 => out.extend_from_slice(format!("\x1b[1;{}{}", xm, (b'P' + n - 1) as char).as_bytes()),
            5 => tilde(15, &mut out),
            6..=10 => tilde(n + 11, &mut out),
            11 | 12 => tilde(n + 12, &mut out),
            _ => {}
        },
        Code::FocusIn => out.extend_from_slice(b"\x1b[I"),
        Code::FocusOut => out.extend_from_slice(b"\x1b[O"),
        Code::Mouse(_) | Code::Any => {}
    }
    out
}

/// Encode a mouse event for a pane according to its requested encoding.
pub fn encode_mouse(m: &MouseEv, x: u16, y: u16, modes: &Modes) -> Option<Vec<u8>> {
    let motion = m.b & 32 != 0;
    match modes.mouse {
        0 => return None,
        9 if m.release || motion || m.b & 64 != 0 => return None,
        1000 if motion => return None,
        1002 if motion && m.b & 3 == 3 => return None,
        _ => {}
    }
    if modes.mouse_enc == 1006 {
        return Some(format!("\x1b[<{};{};{}{}", m.b, x + 1, y + 1, if m.release { 'm' } else { 'M' }).into_bytes());
    }
    let b = if m.release { 3 } else { m.b };
    if x > 222 || y > 222 {
        return None;
    }
    Some(vec![0x1b, b'[', b'M', (b + 32) as u8, (x + 33) as u8, (y + 33) as u8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_roundtrip() {
        for n in ["C-a", "M-x", "Enter", "Space", "\\", "]", "%", "\"", "F5", "C-Up", "S-Left", "PPage", "BSpace", "MouseDown1Pane"] {
            let k = parse_key_name(n).unwrap_or_else(|| panic!("parse {n}"));
            let back = key_name(k);
            assert_eq!(parse_key_name(&back), Some(k), "{n} -> {back}");
        }
        assert_eq!(parse_key_name("C-A"), parse_key_name("C-a"));
        assert_eq!(parse_key_name("^a"), parse_key_name("C-a"));
        assert_eq!(parse_key_name("C-i"), parse_key_name("Tab"));
        assert_ne!(parse_key_name("a"), parse_key_name("A"));
    }

    #[test]
    fn parse_input() {
        let mut p = Parser::default();
        let evs = p.feed(b"\x01a\x1b[A\x1b[1;5C\x1bx\x1b[200~pa\x1bste\x1b[201~\x1b[<0;10;5M");
        let keys: Vec<String> = evs
            .iter()
            .map(|e| match e {
                Ev::Key(k, _) => key_name(*k),
                Ev::Paste(b) => format!("paste:{}", String::from_utf8_lossy(b)),
                Ev::Mouse(m) => format!("mouse:{},{},{}", m.b, m.x, m.y),
                Ev::Raw(r) => format!("raw:{:?}", r),
            })
            .collect();
        assert_eq!(keys, vec!["C-a", "a", "Up", "C-Right", "M-x", "paste:pa\x1bste", "mouse:0,9,4"]);
    }

    #[test]
    fn lone_escape_needs_timeout() {
        let mut p = Parser::default();
        assert!(p.feed(b"\x1b").is_empty());
        let e = p.timeout();
        assert_eq!(e.len(), 1);
        assert!(matches!(e[0], Ev::Key(Key { code: Code::Escape, .. }, _)));
    }

    #[test]
    fn split_sequences() {
        let mut p = Parser::default();
        assert!(p.feed(b"\x1b[1;").is_empty());
        let e = p.feed(b"5A");
        assert!(matches!(e[0], Ev::Key(Key { code: Code::Up, mods: CTRL }, _)));
        assert!(p.feed("\u{e9}".as_bytes()[..1].as_ref()).is_empty());
        let e = p.feed(&"\u{e9}".as_bytes()[1..]);
        assert!(matches!(e[0], Ev::Key(Key { code: Code::Char('é'), .. }, _)));
    }

    #[test]
    fn encode_keys() {
        let mut m = Modes::default();
        assert_eq!(encode(parse_key_name("C-c").unwrap(), &m), vec![3]);
        assert_eq!(encode(parse_key_name("Up").unwrap(), &m), b"\x1b[A");
        m.app_cursor = true;
        assert_eq!(encode(parse_key_name("Up").unwrap(), &m), b"\x1bOA");
        assert_eq!(encode(parse_key_name("C-Up").unwrap(), &m), b"\x1b[1;5A");
        assert_eq!(encode(parse_key_name("M-a").unwrap(), &m), b"\x1ba");
        assert_eq!(encode(parse_key_name("Enter").unwrap(), &m), b"\r");
        assert_eq!(encode(parse_key_name("F5").unwrap(), &m), b"\x1b[15~");
    }
}
