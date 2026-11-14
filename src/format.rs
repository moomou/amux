//! tmux formats (`#{...}`, `#S`-style shorthands, strftime) and styles (`#[...]`).

use crate::cell::*;

pub trait Vars {
    fn var(&self, name: &str) -> Option<String>;
    /// Output of `#(cmd)` (cached, asynchronous).
    fn shell(&self, _cmd: &str) -> String {
        String::new()
    }
}

impl Vars for std::collections::HashMap<String, String> {
    fn var(&self, name: &str) -> Option<String> {
        self.get(name).cloned()
    }
}

fn truthy(s: &str) -> bool {
    !s.is_empty() && s != "0"
}

/// Find the matching `}` for a `#{` starting at `start` (index after `{`).
fn matching_brace(c: &[char], start: usize) -> Option<usize> {
    let mut depth = 1;
    let mut i = start;
    while i < c.len() {
        if c[i] == '#' && c.get(i + 1) == Some(&'{') {
            depth += 1;
            i += 2;
            continue;
        }
        if c[i] == '}' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Split at top-level commas (not inside nested `#{}`; `#,` is a literal comma).
fn split_commas(s: &str) -> Vec<String> {
    let c: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0;
    let mut i = 0;
    while i < c.len() {
        if c[i] == '#' && c.get(i + 1) == Some(&'{') {
            depth += 1;
            cur.push_str("#{");
            i += 2;
            continue;
        }
        if c[i] == '#' && c.get(i + 1) == Some(&',') {
            cur.push_str("#,");
            i += 2;
            continue;
        }
        if c[i] == '}' && depth > 0 {
            depth -= 1;
        }
        if c[i] == ',' && depth == 0 {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c[i]);
        }
        i += 1;
    }
    out.push(cur);
    out
}

fn fnmatch(pat: &str, s: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = s.chars().collect();
    fn m(p: &[char], t: &[char]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some('*') => (0..=t.len()).any(|i| m(&p[1..], &t[i..])),
            Some('?') => !t.is_empty() && m(&p[1..], &t[1..]),
            Some(&c) => !t.is_empty() && t[0] == c && m(&p[1..], &t[1..]),
        }
    }
    m(&p, &t)
}

fn short_var(c: char) -> Option<&'static str> {
    Some(match c {
        'S' => "session_name",
        'I' => "window_index",
        'W' => "window_name",
        'P' => "pane_index",
        'T' => "pane_title",
        'H' => "host",
        'h' => "host_short",
        'D' => "pane_id",
        'F' => "window_flags",
        _ => return None,
    })
}

pub fn expand(fmt: &str, v: &dyn Vars) -> String {
    expand_depth(fmt, v, 0)
}

fn expand_depth(fmt: &str, v: &dyn Vars, depth: usize) -> String {
    if depth > 10 {
        return String::new();
    }
    let c: Vec<char> = fmt.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < c.len() {
        if c[i] != '#' {
            out.push(c[i]);
            i += 1;
            continue;
        }
        let n = match c.get(i + 1) {
            Some(&n) => n,
            None => {
                out.push('#');
                break;
            }
        };
        match n {
            '#' => {
                out.push('#');
                i += 2;
            }
            ',' => {
                out.push(',');
                i += 2;
            }
            '[' => {
                // style: keep verbatim (expanding formats inside)
                let end = c[i..].iter().position(|&x| x == ']').map(|p| i + p);
                match end {
                    Some(e) => {
                        let inner: String = c[i + 2..e].iter().collect();
                        out.push_str("#[");
                        out.push_str(&expand_depth(&inner, v, depth + 1));
                        out.push(']');
                        i = e + 1;
                    }
                    None => {
                        out.push('#');
                        i += 1;
                    }
                }
            }
            '(' => {
                let mut d = 1;
                let mut j = i + 2;
                while j < c.len() && d > 0 {
                    if c[j] == '(' {
                        d += 1;
                    } else if c[j] == ')' {
                        d -= 1;
                    }
                    j += 1;
                }
                let cmd: String = c[i + 2..j.saturating_sub(1)].iter().collect();
                out.push_str(&v.shell(&cmd));
                i = j;
            }
            '{' => match matching_brace(&c, i + 2) {
                Some(e) => {
                    let inner: String = c[i + 2..e].iter().collect();
                    out.push_str(&expand_expr(&inner, v, depth));
                    i = e + 1;
                }
                None => {
                    out.push('#');
                    i += 1;
                }
            },
            x => {
                if let Some(name) = short_var(x) {
                    out.push_str(&v.var(name).unwrap_or_default());
                } else {
                    out.push('#');
                    out.push(x);
                }
                i += 2;
            }
        }
    }
    out
}

fn expand_expr(e: &str, v: &dyn Vars, depth: usize) -> String {
    let ex = |s: &str| expand_depth(s, v, depth + 1);
    // conditional
    if let Some(rest) = e.strip_prefix('?') {
        let parts = split_commas(rest);
        let cond = parts.first().cloned().unwrap_or_default();
        let cv = if cond.contains("#{") || cond.starts_with('#') { ex(&cond) } else { v.var(&cond).unwrap_or_default() };
        let (a, b) = (parts.get(1).cloned().unwrap_or_default(), parts.get(2).cloned().unwrap_or_default());
        return if truthy(&cv) { ex(&a) } else { ex(&b) };
    }
    // modifiers: name:arg
    if let Some(colon) = find_modifier_colon(e) {
        let (m, rest) = (&e[..colon], &e[colon + 1..]);
        let val = |s: &str| -> String { if s.contains("#{") || s.starts_with('#') { ex(s) } else { v.var(s).unwrap_or_else(|| ex(s)) } };
        match m {
            "==" | "!=" | "<" | ">" | "<=" | ">=" | "||" | "&&" => {
                let p = split_commas(rest);
                let a = val(p.first().map(|s| s.as_str()).unwrap_or(""));
                let b = val(p.get(1).map(|s| s.as_str()).unwrap_or(""));
                let r = match m {
                    "==" => a == b,
                    "!=" => a != b,
                    "<" => a < b,
                    ">" => a > b,
                    "<=" => a <= b,
                    ">=" => a >= b,
                    "||" => truthy(&a) || truthy(&b),
                    _ => truthy(&a) && truthy(&b),
                };
                return if r { "1".into() } else { "0".into() };
            }
            "m" | "m/r" => {
                let p = split_commas(rest);
                let pat = ex(p.first().map(|s| s.as_str()).unwrap_or(""));
                let s = val(p.get(1).map(|s| s.as_str()).unwrap_or(""));
                return if fnmatch(&pat, &s) { "1".into() } else { "0".into() };
            }
            "l" => return rest.to_string(),
            "E" => return ex(&val(rest)),
            "T" => return strftime(&ex(&val(rest))),
            "b" => return val(rest).rsplit('/').next().unwrap_or("").to_string(),
            "d" => {
                let s = val(rest);
                return match s.rfind('/') {
                    Some(0) => "/".into(),
                    Some(p) => s[..p].to_string(),
                    None => ".".into(),
                };
            }
            "n" => return val(rest).chars().count().to_string(),
            "w" => return crate::cell::str_width(&val(rest)).to_string(),
            "t" => {
                let s = val(rest);
                return s.parse::<i64>().map(|t| strftime_at("%a %b %e %H:%M:%S %Y", t)).unwrap_or(s);
            }
            "q" => return crate::util::shell_quote(&val(rest)),
            _ => {}
        }
        if let Some(n) = m.strip_prefix('=') {
            let n: i64 = n.trim_start_matches('/').split('/').next().unwrap_or("0").parse().unwrap_or(0);
            let s = val(rest);
            let chars: Vec<char> = s.chars().collect();
            let k = n.unsigned_abs() as usize;
            return if chars.len() <= k {
                s
            } else if n >= 0 {
                chars[..k].iter().collect()
            } else {
                chars[chars.len() - k..].iter().collect()
            };
        }
        if let Some(n) = m.strip_prefix('p') {
            let n: i64 = n.parse().unwrap_or(0);
            let s = val(rest);
            let w = s.chars().count() as i64;
            let pad = (n.abs() - w).max(0) as usize;
            return if n >= 0 { format!("{}{}", s, " ".repeat(pad)) } else { format!("{}{}", " ".repeat(pad), s) };
        }
        if let Some(spec) = m.strip_prefix("s/") {
            let parts: Vec<&str> = spec.splitn(3, '/').collect();
            if parts.len() >= 2 {
                return val(rest).replace(parts[0], parts[1]);
            }
        }
        if m.starts_with("e|") {
            let op = m.trim_start_matches("e|").trim_end_matches('|');
            let p = split_commas(rest);
            let a: f64 = val(p.first().map(|s| s.as_str()).unwrap_or("0")).parse().unwrap_or(0.0);
            let b: f64 = val(p.get(1).map(|s| s.as_str()).unwrap_or("0")).parse().unwrap_or(0.0);
            let r = match op {
                "+" => a + b,
                "-" => a - b,
                "*" => a * b,
                "/" if b != 0.0 => a / b,
                "%" if b != 0.0 => a % b,
                _ => 0.0,
            };
            return format!("{}", r as i64);
        }
    }
    v.var(e).unwrap_or_default()
}

fn find_modifier_colon(e: &str) -> Option<usize> {
    // A modifier is a short prefix before ':' with no '#' or '{'.
    let pos = e.find(':')?;
    let m = &e[..pos];
    if m.is_empty() || m.len() > 16 || m.contains('#') || m.contains('{') || m.contains(' ') {
        return None;
    }
    Some(pos)
}

pub fn strftime(fmt: &str) -> String {
    strftime_at(fmt, crate::util::now_unix() as i64)
}

pub fn strftime_at(fmt: &str, t: i64) -> String {
    if !fmt.contains('%') {
        return fmt.to_string();
    }
    let Ok(cfmt) = std::ffi::CString::new(fmt) else { return fmt.to_string() };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let tt = t as libc::time_t;
    let mut buf = vec![0u8; 512];
    // SAFETY: valid pointers; localtime_r/strftime write within the provided buffers.
    let n = unsafe {
        libc::localtime_r(&tt, &mut tm);
        libc::strftime(buf.as_mut_ptr() as *mut libc::c_char, buf.len(), cfmt.as_ptr(), &tm)
    };
    buf.truncate(n);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Colour by tmux name; `None` if not recognised.
pub fn parse_colour(s: &str) -> Option<u32> {
    let s = s.trim().to_ascii_lowercase();
    let named = ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"];
    if s == "default" || s == "terminal" {
        return Some(0);
    }
    if let Some(i) = named.iter().position(|&n| n == s) {
        return Some(color_idx(i as u8));
    }
    if let Some(r) = s.strip_prefix("bright")
        && let Some(i) = named.iter().position(|&n| n == r)
    {
        return Some(color_idx(i as u8 + 8));
    }
    if let Some(n) = s.strip_prefix("colour").or_else(|| s.strip_prefix("color")) {
        return n.parse::<u8>().ok().map(color_idx);
    }
    if let Some(h) = s.strip_prefix('#')
        && h.len() == 6
    {
        let v = u32::from_str_radix(h, 16).ok()?;
        return Some(color_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8));
    }
    if let Ok(n) = s.parse::<u8>() {
        return Some(color_idx(n));
    }
    None
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    None,
    Left,
    Centre,
    Right,
}

/// Apply a style string (e.g. "fg=red,bg=default,bold") to `base`.
pub fn apply_style(spec: &str, mut st: Style) -> (Style, Align, Option<u32>) {
    let mut align = Align::None;
    let mut fill = None;
    for part in spec.split([',', ' ']).map(|p| p.trim()).filter(|p| !p.is_empty()) {
        let (k, val) = part.split_once('=').unwrap_or((part, ""));
        match k {
            "fg" => {
                if let Some(c) = parse_colour(val) {
                    st.set_fg(c)
                }
            }
            "bg" => {
                if let Some(c) = parse_colour(val) {
                    st.set_bg(c)
                }
            }
            "fill" => fill = parse_colour(val),
            "align" => {
                align = match val {
                    "left" => Align::Left,
                    "centre" | "center" => Align::Centre,
                    "right" => Align::Right,
                    _ => Align::None,
                }
            }
            "default" | "none" => st = Style::DEFAULT,
            "bright" | "bold" => st.fg |= BOLD,
            "nobright" | "nobold" => st.fg &= !BOLD,
            "dim" => st.fg |= DIM,
            "nodim" => st.fg &= !DIM,
            "italics" => st.fg |= ITALIC,
            "noitalics" => st.fg &= !ITALIC,
            "underscore" => st.set_underline(1),
            "double-underscore" => st.set_underline(2),
            "curly-underscore" => st.set_underline(3),
            "dotted-underscore" => st.set_underline(4),
            "dashed-underscore" => st.set_underline(5),
            "nounderscore" => st.set_underline(0),
            "blink" => st.fg |= BLINK,
            "noblink" => st.fg &= !BLINK,
            "reverse" => st.fg |= REVERSE,
            "noreverse" => st.fg &= !REVERSE,
            "hidden" => st.fg |= HIDDEN,
            "nohidden" => st.fg &= !HIDDEN,
            "strikethrough" => st.bg |= STRIKE,
            "nostrikethrough" => st.bg &= !STRIKE,
            "overline" => st.bg |= OVERLINE,
            "nooverline" => st.bg &= !OVERLINE,
            _ => {}
        }
    }
    (st, align, fill)
}

/// A run of styled text with an alignment section.
#[derive(Clone, Debug)]
pub struct Span {
    pub style: Style,
    pub text: String,
    pub align: Align,
}

/// Split expanded text into styled spans by `#[...]` directives.
pub fn spans(s: &str, base: Style) -> (Vec<Span>, Option<u32>) {
    let mut out = Vec::new();
    let mut st = base;
    let mut align = Align::None;
    let mut fill = None;
    let mut cur = String::new();
    let c: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < c.len() {
        if c[i] == '#'
            && c.get(i + 1) == Some(&'[')
            && let Some(end) = c[i..].iter().position(|&x| x == ']')
        {
            if !cur.is_empty() {
                out.push(Span { style: st, text: std::mem::take(&mut cur), align });
            }
            let inner: String = c[i + 2..i + end].iter().collect();
            if inner.trim() == "default" {
                st = base;
            } else {
                let (ns, a, f) = apply_style(&inner, st);
                st = ns;
                if a != Align::None {
                    align = a;
                }
                if f.is_some() {
                    fill = f;
                }
            }
            i += end + 1;
            continue;
        }
        cur.push(c[i]);
        i += 1;
    }
    if !cur.is_empty() {
        out.push(Span { style: st, text: cur, align });
    }
    (out, fill)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn vars() -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("session_name".into(), "work".into());
        m.insert("window_index".into(), "1".into());
        m.insert("window_name".into(), "vim".into());
        m.insert("window_flags".into(), "*".into());
        m.insert("pane_title".into(), "a very long pane title here".into());
        m.insert("pane_current_path".into(), "/home/u/src/proj".into());
        m.insert("zero".into(), "0".into());
        m
    }

    #[test]
    fn expands() {
        let v = vars();
        assert_eq!(expand("[#{session_name}] ", &v), "[work] ");
        assert_eq!(expand("#I:#W#{?window_flags,#{window_flags}, }", &v), "1:vim*");
        assert_eq!(expand("#{?zero,yes,no}", &v), "no");
        assert_eq!(expand("\"#{=6:pane_title}\"", &v), "\"a very\"");
        assert_eq!(expand("#{=-4:pane_title}", &v), "here");
        assert_eq!(expand("#{b:pane_current_path}", &v), "proj");
        assert_eq!(expand("#{==:#{session_name},work}", &v), "1");
        assert_eq!(expand("##", &v), "#");
        assert_eq!(expand("#{?#{==:#{window_name},vim},V,-}", &v), "V");
        assert_eq!(expand("#[fg=red]x", &v), "#[fg=red]x");
    }

    #[test]
    fn styles() {
        let (st, _, _) = apply_style("bg=green,fg=black", Style::DEFAULT);
        assert_eq!(st.bg_color(), Color::Idx(2));
        assert_eq!(st.fg_color(), Color::Idx(0));
        let (st, _, _) = apply_style("bg=colour123,fg=#ff0000,bold", Style::DEFAULT);
        assert_eq!(st.bg_color(), Color::Idx(123));
        assert_eq!(st.fg_color(), Color::Rgb(255, 0, 0));
        assert!(st.has(BOLD));
        let (sp, _) = spans("a#[fg=red]b#[default]c", Style::DEFAULT);
        assert_eq!(sp.len(), 3);
        assert_eq!(sp[1].style.fg_color(), Color::Idx(1));
        assert_eq!(sp[2].style, Style::DEFAULT);
    }

    #[test]
    fn time() {
        assert_eq!(strftime_at("%Y", 0).len(), 4);
    }
}
