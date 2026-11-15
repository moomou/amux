//! Key tables, key dispatch (prefix/repeat/copy-mode), mouse, prompts and
//! overlay navigation.

use super::{Client, Overlay, Prompt, Server, TreeItem};
use crate::keys::{self, Code, Ev, Key, MouseEv, parse_key_name};
use crate::model::Rect;
use crate::proto::{Enc, s2k};
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct Binding {
    pub cmds: Vec<Vec<String>>,
    pub repeat: bool,
    pub note: String,
}

#[derive(Default)]
pub struct KeyTables {
    pub tables: HashMap<String, HashMap<Key, Binding>>,
}

pub enum Drag {
    Pane { pane: u32, x: u16, y: u16, moved: bool },
    Border { pane: u32, vertical: bool, last: u16 },
}

fn cmdline(s: &str) -> Vec<Vec<String>> {
    crate::cmd::parse_cmdlist(s, &|_| None).unwrap_or_default()
}

impl KeyTables {
    pub fn bind(&mut self, table: &str, key: Key, cmds: Vec<Vec<String>>, repeat: bool, note: &str) {
        self.tables.entry(table.to_string()).or_default().insert(key, Binding { cmds, repeat, note: note.to_string() });
    }

    pub fn unbind(&mut self, table: &str, key: Key) {
        if let Some(t) = self.tables.get_mut(table) {
            t.remove(&key);
        }
    }

    pub fn lookup(&self, table: &str, key: Key) -> Option<&Binding> {
        let t = self.tables.get(table)?;
        t.get(&key).or_else(|| t.get(&Key::new(Code::Any, 0)))
    }

    pub fn default_tables() -> KeyTables {
        let mut kt = KeyTables::default();
        let mut b = |table: &str, key: &str, cmd: &str, repeat: bool| {
            let k = parse_key_name(key).unwrap_or_else(|| panic!("bad default key {key}"));
            kt.bind(table, k, cmdline(cmd), repeat, "");
        };
        // prefix table (tmux 3.2 defaults)
        let p = "prefix";
        b(p, "C-b", "send-prefix", false);
        b(p, "C-o", "rotate-window", false);
        b(p, "C-z", "suspend-client", false);
        b(p, "Space", "next-layout", false);
        b(p, "!", "break-pane", false);
        b(p, "\"", "split-window", false);
        b(p, "#", "list-buffers", false);
        b(p, "$", "command-prompt -I \"#S\" \"rename-session -- '%%'\"", false);
        b(p, "%", "split-window -h", false);
        b(p, "&", "confirm-before -p \"kill-window #W? (y/n)\" kill-window", false);
        b(p, "'", "command-prompt -p index \"select-window -t ':%%'\"", false);
        b(p, "(", "switch-client -p", false);
        b(p, ")", "switch-client -n", false);
        b(p, ",", "command-prompt -I \"#W\" \"rename-window -- '%%'\"", false);
        b(p, "-", "delete-buffer", false);
        b(p, ".", "command-prompt \"move-window -t '%%'\"", false);
        for i in 0..10 {
            let k = i.to_string();
            b(p, &k, &format!("select-window -t :={}", i), false);
        }
        b(p, ":", "command-prompt", false);
        b(p, ";", "last-pane", false);
        b(p, "=", "choose-buffer", false);
        b(p, "?", "list-keys", false);
        b(p, "D", "choose-client", false);
        b(p, "L", "switch-client -l", false);
        b(p, "[", "copy-mode", false);
        b(p, "]", "paste-buffer", false);
        b(p, "c", "new-window", false);
        b(p, "d", "detach-client", false);
        b(p, "f", "command-prompt \"find-window -- '%%'\"", false);
        b(p, "i", "display-message", false);
        b(p, "l", "last-window", false);
        b(p, "n", "next-window", false);
        b(p, "o", "select-pane -t :.+", false);
        b(p, "p", "previous-window", false);
        b(p, "q", "display-panes", false);
        b(p, "r", "refresh-client", false);
        b(p, "s", "choose-tree -s", false);
        b(p, "t", "clock-mode", false);
        b(p, "w", "choose-tree -w", false);
        b(p, "x", "confirm-before -p \"kill-pane #P? (y/n)\" kill-pane", false);
        b(p, "z", "resize-pane -Z", false);
        b(p, "{", "swap-pane -U", false);
        b(p, "}", "swap-pane -D", false);
        b(p, "~", "show-messages", false);
        b(p, "PPage", "copy-mode -u", false);
        b(p, "Up", "select-pane -U", true);
        b(p, "Down", "select-pane -D", true);
        b(p, "Left", "select-pane -L", true);
        b(p, "Right", "select-pane -R", true);
        b(p, "M-1", "select-layout even-horizontal", false);
        b(p, "M-2", "select-layout even-vertical", false);
        b(p, "M-3", "select-layout main-horizontal", false);
        b(p, "M-4", "select-layout main-vertical", false);
        b(p, "M-5", "select-layout tiled", false);
        b(p, "M-n", "next-window -a", false);
        b(p, "M-o", "rotate-window -D", false);
        b(p, "M-p", "previous-window -a", false);
        b(p, "C-Up", "resize-pane -U", true);
        b(p, "C-Down", "resize-pane -D", true);
        b(p, "C-Left", "resize-pane -L", true);
        b(p, "C-Right", "resize-pane -R", true);
        b(p, "M-Up", "resize-pane -U 5", true);
        b(p, "M-Down", "resize-pane -D 5", true);
        b(p, "M-Left", "resize-pane -L 5", true);
        b(p, "M-Right", "resize-pane -R 5", true);
        // amux additions (keys unused by tmux defaults)
        b(p, "A", "agent-view", false);
        b(p, "N", "agent-next", false);
        b(p, "P", "plugin view", false);
        // copy-mode-vi
        let v = "copy-mode-vi";
        for (k, c) in [
            ("C-b", "page-up"),
            ("C-c", "cancel"),
            ("C-d", "halfpage-down"),
            ("C-e", "scroll-down"),
            ("C-f", "page-down"),
            ("C-h", "cursor-left"),
            ("C-j", "copy-selection-and-cancel"),
            ("Enter", "copy-selection-and-cancel"),
            ("C-u", "halfpage-up"),
            ("C-v", "rectangle-toggle"),
            ("C-y", "scroll-up"),
            ("Escape", "clear-selection"),
            ("Space", "begin-selection"),
            ("$", "end-of-line"),
            (",", "jump-reverse"),
            ("0", "start-of-line"),
            (";", "jump-again"),
            ("B", "previous-space"),
            ("D", "copy-end-of-line"),
            ("E", "next-space-end"),
            ("G", "history-bottom"),
            ("H", "top-line"),
            ("J", "scroll-down"),
            ("K", "scroll-up"),
            ("L", "bottom-line"),
            ("M", "middle-line"),
            ("N", "search-reverse"),
            ("V", "select-line"),
            ("W", "next-space"),
            ("^", "back-to-indentation"),
            ("b", "previous-word"),
            ("e", "next-word-end"),
            ("g", "history-top"),
            ("h", "cursor-left"),
            ("j", "cursor-down"),
            ("k", "cursor-up"),
            ("l", "cursor-right"),
            ("n", "search-again"),
            ("o", "other-end"),
            ("q", "cancel"),
            ("v", "rectangle-toggle"),
            ("w", "next-word"),
            ("Up", "cursor-up"),
            ("Down", "cursor-down"),
            ("Left", "cursor-left"),
            ("Right", "cursor-right"),
            ("Home", "start-of-line"),
            ("End", "end-of-line"),
            ("NPage", "page-down"),
            ("PPage", "page-up"),
            ("BSpace", "cursor-left"),
            ("[", "previous-prompt"),
            ("]", "next-prompt"),
        ] {
            b(v, k, &format!("send-keys -X {}", c), false);
        }
        b(v, "/", "command-prompt -p \"(search down)\" \"send -X search-forward '%%'\"", false);
        b(v, "?", "command-prompt -p \"(search up)\" \"send -X search-backward '%%'\"", false);
        b(v, ":", "command-prompt -p \"(goto line)\" \"send -X goto-line '%%'\"", false);
        b(v, "f", "command-prompt -1 -p \"(jump forward)\" \"send -X jump-forward '%%'\"", false);
        b(v, "F", "command-prompt -1 -p \"(jump backward)\" \"send -X jump-backward '%%'\"", false);
        b(v, "t", "command-prompt -1 -p \"(jump to forward)\" \"send -X jump-to-forward '%%'\"", false);
        b(v, "T", "command-prompt -1 -p \"(jump to backward)\" \"send -X jump-to-backward '%%'\"", false);
        // copy-mode (emacs)
        let e = "copy-mode";
        for (k, c) in [
            ("C-Space", "begin-selection"),
            ("C-a", "start-of-line"),
            ("C-b", "cursor-left"),
            ("C-c", "cancel"),
            ("C-e", "end-of-line"),
            ("C-f", "cursor-right"),
            ("C-g", "clear-selection"),
            ("C-k", "copy-end-of-line-and-cancel"),
            ("C-n", "cursor-down"),
            ("C-p", "cursor-up"),
            ("C-v", "page-down"),
            ("C-w", "copy-selection-and-cancel"),
            ("Escape", "cancel"),
            ("M-v", "page-up"),
            ("M-w", "copy-selection-and-cancel"),
            ("M-<", "history-top"),
            ("M->", "history-bottom"),
            ("M-b", "previous-word"),
            ("M-f", "next-word-end"),
            ("q", "cancel"),
            ("Up", "cursor-up"),
            ("Down", "cursor-down"),
            ("Left", "cursor-left"),
            ("Right", "cursor-right"),
            ("NPage", "page-down"),
            ("PPage", "page-up"),
            ("Home", "start-of-line"),
            ("End", "end-of-line"),
        ] {
            b(e, k, &format!("send-keys -X {}", c), false);
        }
        b(e, "C-r", "command-prompt -p \"(search up)\" \"send -X search-backward '%%'\"", false);
        b(e, "C-s", "command-prompt -p \"(search down)\" \"send -X search-forward '%%'\"", false);
        b(e, "g", "command-prompt -p \"(goto line)\" \"send -X goto-line '%%'\"", false);
        kt.tables.entry("root".into()).or_default();
        kt
    }
}

impl Server {
    pub fn active_pane(&self, sid: u32) -> Option<u32> {
        let s = self.sessions.iter().find(|s| s.id == sid)?;
        self.windows.get(&s.active).map(|w| w.active)
    }

    pub fn client_input(&mut self, cid: u32, data: &[u8]) {
        let Some(i) = self.client_idx(cid) else { return };
        let evs = self.clients[i].parser.feed(data);
        let pending = self.clients[i].parser.has_pending() && !self.clients[i].parser.in_paste();
        let esc = self.gnum("escape-time", 500).max(0) as u64;
        self.clients[i].esc_deadline = if pending { Some(Instant::now() + Duration::from_millis(esc)) } else { None };
        self.clients[i].activity = Instant::now();
        for e in evs {
            self.handle_input_event(cid, e);
        }
    }

    pub fn handle_input_event(&mut self, cid: u32, ev: Ev) {
        self.status_gen += 1;
        let Some(i) = self.client_idx(cid) else { return };
        let Some(sid) = self.clients[i].attached else { return };
        match ev {
            Ev::Key(k, raw) => self.handle_key(cid, k, &raw),
            Ev::Paste(b) => {
                if self.clients[i].prompt.is_some() {
                    let text = String::from_utf8_lossy(&b).replace(['\r', '\n'], " ");
                    let p = self.clients[i].prompt.as_mut().unwrap();
                    let at = p.text.char_indices().nth(p.cursor).map(|(i, _)| i).unwrap_or(p.text.len());
                    p.text.insert_str(at, &text);
                    p.cursor += text.chars().count();
                    self.clients[i].dirty = true;
                    return;
                }
                if self.clients[i].overlay.is_some() {
                    return;
                }
                if let Some(pid) = self.active_pane(sid) {
                    self.paste_to_pane(pid, &b);
                }
            }
            Ev::Mouse(m) => self.handle_mouse(cid, m),
            Ev::Raw(b) => {
                if self.clients[i].overlay.is_none()
                    && self.clients[i].prompt.is_none()
                    && let Some(pid) = self.active_pane(sid)
                    && let Some(p) = self.panes.get_mut(&pid)
                    && !p.in_copy
                {
                    p.input(&b);
                }
            }
        }
    }

    pub fn paste_to_pane(&mut self, pid: u32, data: &[u8]) {
        if let Some(p) = self.panes.get_mut(&pid) {
            if p.mirror.modes.bracketed_paste {
                let mut v = b"\x1b[200~".to_vec();
                v.extend_from_slice(data);
                v.extend_from_slice(b"\x1b[201~");
                p.input(&v);
            } else {
                p.input(data);
            }
        }
    }

    fn copy_table(&self, sid: u32) -> &'static str {
        if self.opt(Some(sid), None, "mode-keys") == "vi" { "copy-mode-vi" } else { "copy-mode" }
    }

    fn handle_key(&mut self, cid: u32, k: Key, raw: &[u8]) {
        let Some(i) = self.client_idx(cid) else { return };
        let Some(sid) = self.clients[i].attached else { return };
        if self.clients[i].prompt.is_some() {
            self.prompt_key(cid, k);
            return;
        }
        if self.clients[i].overlay.is_some() {
            self.overlay_key(cid, k);
            return;
        }
        let table = self.clients[i].key_table.clone();
        let prefix = parse_key_name(&self.opt(Some(sid), None, "prefix"));
        let prefix2 = parse_key_name(&self.opt(Some(sid), None, "prefix2"));
        if table == "root" && (Some(k) == prefix || Some(k) == prefix2) {
            self.clients[i].key_table = "prefix".into();
            self.clients[i].dirty = true;
            return;
        }
        if table != "root" {
            let b = self.keys.lookup(&table, k).cloned();
            let was_repeat = self.clients[i].repeat_until.is_some();
            match b {
                Some(b) if !was_repeat || b.repeat => {
                    if b.repeat {
                        let rt = self.gnum("repeat-time", 500).max(0) as u64;
                        self.clients[i].repeat_until = Some(Instant::now() + Duration::from_millis(rt));
                    } else {
                        self.clients[i].key_table = "root".into();
                        self.clients[i].repeat_until = None;
                    }
                    self.clients[i].dirty = true;
                    self.run_binding(cid, b.cmds);
                    return;
                }
                _ => {
                    self.clients[i].key_table = "root".into();
                    self.clients[i].repeat_until = None;
                    self.clients[i].dirty = true;
                    if !was_repeat {
                        return;
                    }
                }
            }
        }
        if let Some(b) = self.keys.lookup("root", k).cloned() {
            self.run_binding(cid, b.cmds);
            return;
        }
        let Some(pid) = self.active_pane(sid) else { return };
        let in_copy = self.panes.get(&pid).map(|p| p.in_copy).unwrap_or(false);
        if in_copy {
            let ct = self.copy_table(sid);
            if let Some(b) = self.keys.lookup(ct, k).cloned() {
                self.run_binding(cid, b.cmds);
            }
            return;
        }
        self.forward_key(sid, pid, k, raw);
    }

    pub fn forward_key(&mut self, sid: u32, pid: u32, k: Key, raw: &[u8]) {
        let sync = self.window_of_pane(pid).map(|w| self.opt(Some(sid), Some(w), "synchronize-panes") == "on").unwrap_or(false);
        let targets: Vec<u32> =
            if sync { self.window_of_pane(pid).and_then(|w| self.windows.get(&w)).map(|w| w.layout.panes()).unwrap_or_default() } else { vec![pid] };
        for t in targets {
            let Some(p) = self.panes.get_mut(&t) else { continue };
            if p.exited.is_some() {
                continue;
            }
            let needs_encode = matches!(k.code, Code::Up | Code::Down | Code::Left | Code::Right | Code::Home | Code::End) || raw.is_empty();
            let bytes = if needs_encode { keys::encode(k, &p.mirror.modes) } else { raw.to_vec() };
            if matches!(k.code, Code::FocusIn | Code::FocusOut) && !p.mirror.modes.focus {
                continue;
            }
            p.input(&bytes);
        }
    }

    pub fn run_binding(&mut self, cid: u32, cmds: Vec<Vec<String>>) {
        let Some(i) = self.client_idx(cid) else { return };
        for (n, c) in cmds.into_iter().enumerate() {
            self.clients[i].queue.insert(n, c);
        }
        self.run_queue(cid);
    }

    // ----- prompt --------------------------------------------------

    pub fn open_prompt(&mut self, cid: u32, label: &str, initial: &str, template: &str, single_key: bool, confirm: bool) {
        let Some(i) = self.client_idx(cid) else { return };
        self.clients[i].prompt = Some(Prompt {
            label: label.to_string(),
            text: initial.to_string(),
            cursor: initial.chars().count(),
            template: template.to_string(),
            single_key,
            confirm,
            history_pos: None,
        });
        self.clients[i].dirty = true;
    }

    fn prompt_key(&mut self, cid: u32, k: Key) {
        let Some(i) = self.client_idx(cid) else { return };
        let mut p = self.clients[i].prompt.take().unwrap();
        self.clients[i].dirty = true;
        if p.confirm {
            if k == Key::ch('y') || k == Key::ch('Y') {
                let cmds = crate::cmd::parse_cmdlist(&p.template, &|n| std::env::var(n).ok()).unwrap_or_default();
                self.run_binding(cid, cmds);
            }
            return;
        }
        if p.single_key {
            if k.code == Code::Escape {
                return;
            }
            let s = match k.code {
                Code::Char(c) if k.mods == 0 => c.to_string(),
                _ => keys::key_name(k),
            };
            self.finish_prompt(cid, &p.template, &s);
            return;
        }
        let ctrl = k.mods & keys::CTRL != 0;
        let mut chars: Vec<char> = p.text.chars().collect();
        match (k.code, ctrl) {
            (Code::Enter, _) => {
                if !p.text.is_empty() {
                    self.prompt_history.push(p.text.clone());
                }
                self.finish_prompt(cid, &p.template, &p.text);
                return;
            }
            (Code::Escape, _) | (Code::Char('c' | 'g'), true) => return,
            (Code::Backspace, _) | (Code::Char('h'), true) => {
                if p.cursor > 0 {
                    chars.remove(p.cursor - 1);
                    p.cursor -= 1;
                } else if chars.is_empty() {
                    return;
                }
            }
            (Code::Delete, _) | (Code::Char('d'), true) => {
                if p.cursor < chars.len() {
                    chars.remove(p.cursor);
                }
            }
            (Code::Left, _) | (Code::Char('b'), true) => p.cursor = p.cursor.saturating_sub(1),
            (Code::Right, _) | (Code::Char('f'), true) => p.cursor = (p.cursor + 1).min(chars.len()),
            (Code::Home, _) | (Code::Char('a'), true) => p.cursor = 0,
            (Code::End, _) | (Code::Char('e'), true) => p.cursor = chars.len(),
            (Code::Char('u'), true) => {
                chars.clear();
                p.cursor = 0;
            }
            (Code::Char('k'), true) => chars.truncate(p.cursor),
            (Code::Char('w'), true) => {
                let mut j = p.cursor;
                while j > 0 && chars[j - 1] == ' ' {
                    j -= 1;
                }
                while j > 0 && chars[j - 1] != ' ' {
                    j -= 1;
                }
                chars.drain(j..p.cursor);
                p.cursor = j;
            }
            (Code::Up | Code::Down, _) => {
                let n = self.prompt_history.len();
                if n > 0 {
                    let pos = match (p.history_pos, k.code) {
                        (None, Code::Up) => Some(n - 1),
                        (Some(x), Code::Up) => Some(x.saturating_sub(1)),
                        (Some(x), Code::Down) if x + 1 < n => Some(x + 1),
                        _ => None,
                    };
                    p.history_pos = pos;
                    chars = pos.map(|x| self.prompt_history[x].chars().collect()).unwrap_or_default();
                    p.cursor = chars.len();
                }
            }
            (Code::Char(c), false) => {
                chars.insert(p.cursor, c);
                p.cursor += 1;
            }
            (Code::Tab, _) => {
                // Complete command names.
                if !chars.contains(&' ') {
                    let cur: String = chars.iter().collect();
                    let cands: Vec<&str> = crate::cmd::PUBLIC_COMMANDS.iter().copied().filter(|c| c.starts_with(&cur)).collect();
                    if cands.len() == 1 {
                        chars = format!("{} ", cands[0]).chars().collect();
                        p.cursor = chars.len();
                    }
                }
            }
            _ => {}
        }
        p.text = chars.into_iter().collect();
        self.clients[i].prompt = Some(p);
    }

    fn finish_prompt(&mut self, cid: u32, template: &str, text: &str) {
        let cmd = if template.is_empty() {
            text.to_string()
        } else {
            let esc = text.replace('\\', "\\\\").replace('\'', "'\\''");
            template.replace("%%%", &esc).replace("%%", &esc).replace("%1", &esc)
        };
        if cmd.trim().is_empty() {
            return;
        }
        match crate::cmd::parse_cmdlist(&cmd, &|n| std::env::var(n).ok()) {
            Ok(cmds) => self.run_binding(cid, cmds),
            Err(e) => self.show_message(cid, &format!("parse error: {}", e)),
        }
    }

    pub fn show_message(&mut self, cid: u32, msg: &str) {
        let t = self.gnum("display-time", 750).max(1) as u64;
        if let Some(i) = self.client_idx(cid) {
            let c = &mut self.clients[i];
            if c.attached.is_some() {
                c.message = Some((msg.to_string(), Instant::now() + Duration::from_millis(t)));
                c.dirty = true;
            } else {
                c.print(msg);
            }
        }
        self.log_message(msg);
    }

    // ----- overlays ------------------------------------------------

    fn overlay_key(&mut self, cid: u32, k: Key) {
        let Some(i) = self.client_idx(cid) else { return };
        self.clients[i].dirty = true;
        let ov = self.clients[i].overlay.take().unwrap();
        let (down, up, quit) = (k.is_nav_down(), k.is_nav_up(), k.is_quit());
        match ov {
            Overlay::Agents { sel, tiles, filter } => {
                let rows = super::render::agent_rows(self, &filter);
                let n = rows.len();
                let mut sel = sel.min(n.saturating_sub(1));
                if quit {
                    return;
                }
                if down || k == Key::ch('l') && tiles {
                    sel = (sel + 1).min(n.saturating_sub(1));
                } else if up || k == Key::ch('h') && tiles {
                    sel = sel.saturating_sub(1);
                } else if k == Key::ch('t') {
                    self.clients[i].overlay = Some(Overlay::Agents { sel, tiles: !tiles, filter });
                    return;
                } else if let Code::Char(c @ '1'..='9') = k.code
                    && k.mods == 0
                {
                    let idx = (c as usize) - ('1' as usize);
                    if let Some(r) = rows.get(idx) {
                        self.jump_to_pane(cid, r.pane);
                    }
                    return;
                } else if k.code == Code::Enter {
                    if let Some(r) = rows.get(sel) {
                        self.jump_to_pane(cid, r.pane);
                    }
                    return;
                } else if k == Key::ch('r') {
                    if let Some(r) = rows.get(sel) {
                        let pid = r.pane;
                        self.clients[i].overlay = Some(Overlay::Agents { sel, tiles, filter });
                        self.open_prompt(
                            cid,
                            &format!("reply to %{}: ", pid),
                            "",
                            &format!("send-keys -t %{} -l '%%' ; send-keys -t %{} Enter", pid, pid),
                            false,
                            false,
                        );
                    }
                    return;
                } else if k == Key::ch('/') {
                    self.clients[i].overlay = Some(Overlay::Agents { sel: 0, tiles, filter: String::new() });
                    self.open_prompt(cid, "filter agents: ", &filter, "agent-view -f '%%'", false, false);
                    return;
                } else if k == Key::ch('x') {
                    if let Some(r) = rows.get(sel) {
                        let pid = r.pane;
                        self.clients[i].overlay = Some(Overlay::Agents { sel, tiles, filter });
                        self.open_prompt(cid, &format!("kill agent pane %{}? (y/n)", pid), "", &format!("kill-pane -t %{}", pid), false, true);
                    }
                    return;
                }
                self.clients[i].overlay = Some(Overlay::Agents { sel, tiles, filter });
            }
            Overlay::Tree { sel, items } => {
                if quit {
                    return;
                }
                let n = items.len();
                let mut sel = sel;
                if down {
                    sel = (sel + 1).min(n.saturating_sub(1));
                } else if up {
                    sel = sel.saturating_sub(1);
                } else if k.code == Code::Enter {
                    if let Some(it) = items.get(sel).cloned() {
                        self.choose_item(cid, &it);
                    }
                    return;
                } else if k == Key::ch('x') {
                    if let Some(it) = items.get(sel).cloned() {
                        let cmd = match it.window {
                            Some(w) => vec!["kill-window".into(), "-t".into(), format!("@{}", w)],
                            None => vec!["kill-session".into(), "-t".into(), format!("${}", it.session)],
                        };
                        self.run_binding(cid, vec![cmd]);
                    }
                    return;
                }
                self.clients[i].overlay = Some(Overlay::Tree { sel, items });
            }
            Overlay::View { title, lines, top } => {
                if quit {
                    return;
                }
                let h = self.clients[i].rows.saturating_sub(2) as usize;
                let max = lines.len().saturating_sub(h);
                let top = k.scroll(top, h, max).unwrap_or(top);
                self.clients[i].overlay = Some(Overlay::View { title, lines, top });
            }
            Overlay::Plugins(v) => self.plugin_view_key(cid, v, k),
            Overlay::Panes { .. } => {
                if let Code::Char(c @ '0'..='9') = k.code {
                    let Some(sid) = self.clients[i].attached else { return };
                    let base = self.gnum("pane-base-index", 0).max(0) as usize;
                    let idx = (c as usize - '0' as usize).wrapping_sub(base);
                    if let Some(s) = self.sessions.iter().find(|s| s.id == sid)
                        && let Some(w) = self.windows.get(&s.active)
                        && let Some(&pid) = w.layout.panes().get(idx)
                    {
                        self.select_pane(pid);
                    }
                }
            }
        }
    }

    fn choose_item(&mut self, cid: u32, it: &TreeItem) {
        let mut cmds = vec![vec!["switch-client".into(), "-t".into(), format!("${}", it.session)]];
        if let Some(w) = it.window {
            cmds.push(vec!["select-window".into(), "-t".into(), format!("@{}", w)]);
        }
        self.run_binding(cid, cmds);
    }

    /// Switch the client to the session/window/pane holding `pid`.
    pub fn jump_to_pane(&mut self, cid: u32, pid: u32) {
        let Some(wid) = self.window_of_pane(pid) else { return };
        let Some(sid) = self.session_of_window(wid) else { return };
        self.select_window(sid, wid);
        self.select_pane(pid);
        self.attach(cid, sid);
    }

    pub fn select_window(&mut self, sid: u32, wid: u32) {
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid)
            && s.active != wid
            && s.windows.iter().any(|(_, w)| *w == wid)
        {
            s.last = Some(s.active);
            s.active = wid;
            s.activity = crate::util::now_unix();
        }
        if let Some(w) = self.windows.get_mut(&wid) {
            w.bell = false;
            w.activity = false;
        }
        self.mark_dirty();
        self.redraw_session(sid);
    }

    pub fn select_pane(&mut self, pid: u32) {
        let Some(wid) = self.window_of_pane(pid) else { return };
        if let Some(w) = self.windows.get_mut(&wid)
            && w.active != pid
        {
            w.last = Some(w.active);
            w.active = pid;
            if w.zoomed {
                w.zoomed = false;
            }
        }
        if let Some(p) = self.panes.get_mut(&pid) {
            p.agent.seen();
        }
        self.resize_window_panes(wid);
        self.mark_dirty();
        self.redraw_all();
    }

    // ----- mouse ---------------------------------------------------

    fn handle_mouse(&mut self, cid: u32, m: MouseEv) {
        let Some(i) = self.client_idx(cid) else { return };
        let Some(sid) = self.clients[i].attached else { return };
        let mouse_on = self.opt(Some(sid), None, "mouse") == "on";
        let Some(sess) = self.sessions.iter().find(|s| s.id == sid) else { return };
        let wid = sess.active;
        let rows = self.clients[i].rows;
        let status_on = self.opt(Some(sid), None, "status") != "off";
        let top = self.opt(Some(sid), None, "status-position") == "top";
        let wy0: u16 = if status_on && top { 1 } else { 0 };
        let status_row = if !status_on {
            None
        } else if top {
            Some(0)
        } else {
            Some(rows - 1)
        };
        let wheel = m.b & 64 != 0;
        let motion = m.b & 32 != 0;
        let btn = m.b & 3;
        if self.clients[i].overlay.is_some() {
            if wheel {
                let k = if btn == 0 { Key::new(Code::Up, 0) } else { Key::new(Code::Down, 0) };
                self.overlay_key(cid, k);
            } else if !motion
                && !m.release
                && btn == 0
                && mouse_on
                && Some(m.y) != status_row
                && let Some(Overlay::Tree { sel, items }) = &self.clients[i].overlay
            {
                let body = rows.saturating_sub(if status_on { 2 } else { 1 }) as usize;
                let start = sel.saturating_sub(body.saturating_sub(1));
                let row = m.y.saturating_sub(wy0) as usize;
                if row >= 1
                    && let Some(it) = items.get(start + row - 1).cloned()
                {
                    self.clients[i].overlay = None;
                    self.choose_item(cid, &it);
                }
            }
            return;
        }
        if Some(m.y) == status_row {
            if !motion && !m.release && btn == 0 && !wheel && mouse_on {
                // Click on a window name: approximate by index order.
                let names: Vec<(u32, String)> = sess.windows.iter().map(|(idx, w)| (*w, format!("{}:{}", idx, self.window_name(*w)))).collect();
                let mut x = self
                    .opt(Some(sid), None, "status-left-length")
                    .parse::<u16>()
                    .unwrap_or(10)
                    .min(crate::cell::str_width(&format!("[{}] ", sess.name)) as u16);
                for (w, n) in names {
                    let wlen = crate::cell::str_width(&n) as u16 + 2;
                    if m.x >= x && m.x < x + wlen {
                        self.select_window(sid, w);
                        break;
                    }
                    x += wlen;
                }
            }
            return;
        }
        let (x, y) = (m.x, m.y.saturating_sub(wy0));
        let rects = self.pane_rects(wid);
        let hit = rects.iter().find(|(_, r)| r.contains(x, y)).copied();
        // Pass-through when mouse is off: only to an app that wants it.
        if !mouse_on {
            if let Some((pid, r)) = hit
                && let Some(p) = self.panes.get_mut(&pid)
                && let Some(b) = keys::encode_mouse(&m, x - r.x, y - r.y, &p.mirror.modes)
            {
                p.input(&b);
            }
            return;
        }
        // Continue drags.
        if let Some(drag) = self.clients[i].drag.take() {
            match drag {
                Drag::Border { pane, vertical, last } => {
                    if m.release {
                        return;
                    }
                    let pos = if vertical { x } else { y };
                    if pos != last {
                        let (dir, n) = if vertical {
                            if pos > last { (crate::model::Dir::R, pos - last) } else { (crate::model::Dir::L, last - pos) }
                        } else if pos > last {
                            (crate::model::Dir::D, pos - last)
                        } else {
                            (crate::model::Dir::U, last - pos)
                        };
                        self.resize_pane(pane, dir, n);
                    }
                    self.clients[i].drag = Some(Drag::Border { pane, vertical, last: pos });
                    return;
                }
                Drag::Pane { pane, x: sx, y: sy, moved } => {
                    let r = rects.iter().find(|(p, _)| *p == pane).map(|(_, r)| *r).unwrap_or_default();
                    let (px, py) = (x.saturating_sub(r.x).min(r.w.saturating_sub(1)), y.saturating_sub(r.y).min(r.h.saturating_sub(1)));
                    let (wants, in_copy) = self.panes.get(&pane).map(|p| (p.mirror.modes.mouse != 0, p.in_copy)).unwrap_or((false, false));
                    if wants && !in_copy {
                        if let Some(p) = self.panes.get_mut(&pane)
                            && let Some(b) = keys::encode_mouse(&m, px, py, &p.mirror.modes)
                        {
                            p.input(&b);
                        }
                        if !m.release {
                            self.clients[i].drag = Some(Drag::Pane { pane, x: sx, y: sy, moved: true });
                        }
                        return;
                    }
                    if m.release {
                        if moved && in_copy {
                            self.copy_cmd(pane, &["copy-selection-and-cancel"]);
                        }
                        return;
                    }
                    if motion {
                        if !in_copy {
                            self.enter_copy(pane, false, 0);
                            let (ox, oy) = (sx.saturating_sub(r.x), sy.saturating_sub(r.y));
                            self.copy_cmd(pane, &["mouse-down", &ox.to_string(), &oy.to_string()]);
                        }
                        self.copy_cmd(pane, &["mouse-drag", &px.to_string(), &py.to_string()]);
                        self.clients[i].drag = Some(Drag::Pane { pane, x: sx, y: sy, moved: true });
                        return;
                    }
                    self.clients[i].drag = Some(Drag::Pane { pane, x: sx, y: sy, moved });
                }
            }
        }
        if m.release {
            return;
        }
        if wheel {
            let Some((pid, r)) = hit else { return };
            let up = btn == 0;
            let (wants, in_copy, alt) =
                self.panes.get(&pid).map(|p| (p.mirror.modes.mouse != 0, p.in_copy, p.mirror.modes.alt_screen)).unwrap_or((false, false, false));
            if wants && !in_copy {
                if let Some(p) = self.panes.get_mut(&pid)
                    && let Some(b) = keys::encode_mouse(&m, x - r.x, y - r.y, &p.mirror.modes)
                {
                    p.input(&b);
                }
            } else if alt && !in_copy {
                // Alternate-scroll: wheel as arrow keys for full-screen apps.
                let k = Key::new(if up { Code::Up } else { Code::Down }, 0);
                for _ in 0..3 {
                    self.forward_key(sid, pid, k, &[]);
                }
            } else if up {
                if in_copy {
                    self.copy_cmd(pid, &["scroll-up", "3"]);
                } else {
                    self.enter_copy(pid, true, 3);
                }
            } else if in_copy {
                self.copy_cmd(pid, &["scroll-down", "3"]);
            }
            return;
        }
        if motion {
            return;
        }
        // Button press.
        match hit {
            Some((pid, r)) => {
                if btn == 0 {
                    self.select_pane(pid);
                }
                let (wants, in_copy) = self.panes.get(&pid).map(|p| (p.mirror.modes.mouse != 0, p.in_copy)).unwrap_or((false, false));
                if wants && !in_copy {
                    if let Some(p) = self.panes.get_mut(&pid)
                        && let Some(b) = keys::encode_mouse(&m, x - r.x, y - r.y, &p.mirror.modes)
                    {
                        p.input(&b);
                    }
                } else if in_copy && btn == 0 {
                    self.copy_cmd(pid, &["mouse-down", &(x - r.x).to_string(), &(y - r.y).to_string()]);
                } else if btn == 1 {
                    // Middle click pastes the top buffer.
                    if let Some((_, t)) = self.buffers.first().cloned() {
                        self.paste_to_pane(pid, t.as_bytes());
                    }
                }
                if btn == 0 {
                    self.clients[i].drag = Some(Drag::Pane { pane: pid, x, y, moved: false });
                }
            }
            None => {
                // Border: find the pane to the left (vertical) or above.
                if btn != 0 {
                    return;
                }
                let left = rects.iter().find(|(_, r)| r.x + r.w == x && y >= r.y && y < r.y + r.h);
                let above = rects.iter().find(|(_, r)| r.y + r.h == y && x >= r.x && x < r.x + r.w);
                if let Some((pid, _)) = left {
                    self.clients[i].drag = Some(Drag::Border { pane: *pid, vertical: true, last: x });
                } else if let Some((pid, _)) = above {
                    self.clients[i].drag = Some(Drag::Border { pane: *pid, vertical: false, last: y });
                }
            }
        }
    }

    pub fn enter_copy(&mut self, pid: u32, scroll_exit: bool, scroll: u32) {
        if let Some(p) = self.panes.get_mut(&pid) {
            let mut e = Enc::new();
            let mut args = vec!["enter".to_string(), if scroll_exit { "1".into() } else { "0".into() }];
            if scroll > 0 {
                args.push(scroll.to_string());
            }
            e.strs(&args);
            p.send(s2k::COPY, &e.buf);
            p.in_copy = true;
        }
    }

    pub fn copy_cmd(&mut self, pid: u32, args: &[&str]) {
        if let Some(p) = self.panes.get_mut(&pid) {
            let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            p.send(s2k::COPY, &Enc::new().strs(&v).buf);
            if args.first() == Some(&"cancel") {
                p.in_copy = false;
            }
        }
    }

    pub fn resize_pane(&mut self, pid: u32, dir: crate::model::Dir, n: u16) {
        let Some(wid) = self.window_of_pane(pid) else { return };
        if let Some(w) = self.windows.get_mut(&wid) {
            let (sx, sy) = (w.sx, w.sy);
            w.zoomed = false;
            w.layout.resize_pane(pid, dir, n, sx, sy);
        }
        self.resize_window_panes(wid);
        self.mark_dirty();
        self.redraw_window(wid);
    }

    pub fn window_name(&self, wid: u32) -> String {
        let Some(w) = self.windows.get(&wid) else { return String::new() };
        let sid = self.session_of_window(wid);
        if w.auto_name
            && self.opt(sid, Some(wid), "automatic-rename") == "on"
            && let Some(p) = self.panes.get(&w.active)
        {
            if p.in_copy {
                return "[amux]".into();
            }
            let mut n = p.current_command();
            if p.exited.is_some() {
                n.push_str("[dead]");
            }
            return n;
        }
        w.name.clone()
    }

    pub fn rect_of(&self, pid: u32) -> Option<Rect> {
        let wid = self.window_of_pane(pid)?;
        self.pane_rects(wid).into_iter().find(|(p, _)| *p == pid).map(|(_, r)| r)
    }
}

impl Client {
    pub fn is_prefix(&self) -> bool {
        self.key_table == "prefix"
    }
}
