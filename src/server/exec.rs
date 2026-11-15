//! Command execution: tmux commands, targets, formats, jobs and waits.

use super::render;
use super::{Client, Job, Overlay, Server, TreeItem, Wait};
use crate::agents::{self, State as AState};
use crate::cmd::{Args, resolve_command};
use crate::format::{self, Vars};
use crate::keys::{self, parse_key_name};
use crate::model::{Dir, Node, PaneMeta, Rect, Session, Window};
use crate::proto::{Enc, s2c, s2k};
use crate::{logf, pty, util};
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

pub enum Flow {
    Next,
    Wait,
}

type R = Result<Flow, String>;

/// Format variable context.
pub struct Fv<'a> {
    pub srv: &'a Server,
    pub client: Option<&'a Client>,
    pub sid: Option<u32>,
    pub wid: Option<u32>,
    pub pid: Option<u32>,
    pub idx: Option<u32>,
}

impl<'a> Fv<'a> {
    pub fn for_client(srv: &'a Server, c: &'a Client) -> Fv<'a> {
        let sid = c.attached;
        let wid = sid.and_then(|s| srv.sessions.iter().find(|x| x.id == s)).map(|s| s.active);
        let pid = wid.and_then(|w| srv.windows.get(&w)).map(|w| w.active);
        Fv { srv, client: Some(c), sid, wid, pid, idx: None }
    }
    pub fn for_pane(srv: &'a Server, client: Option<&'a Client>, pid: u32) -> Fv<'a> {
        let wid = srv.window_of_pane(pid);
        let sid = wid.and_then(|w| srv.session_of_window(w));
        Fv { srv, client, sid, wid, pid: Some(pid), idx: None }
    }
}

impl Fv<'_> {
    /// The pane's rectangle in its window's current layout.
    fn rect(&self) -> Option<Rect> {
        self.srv.rect_of(self.pid?)
    }
}

fn b(v: bool) -> String {
    if v { "1".into() } else { "0".into() }
}

impl Vars for Fv<'_> {
    fn var(&self, name: &str) -> Option<String> {
        let srv = self.srv;
        let sess = self.sid.and_then(|s| srv.sessions.iter().find(|x| x.id == s));
        let win = self.wid.and_then(|w| srv.windows.get(&w));
        let pane = self.pid.and_then(|p| srv.panes.get(&p));
        if name.starts_with('@') {
            return Some(srv.opt(self.sid, self.wid, name));
        }
        let v = match name {
            "session_name" => sess?.name.clone(),
            "session_id" => format!("${}", sess?.id),
            "session_windows" => sess?.windows.len().to_string(),
            "session_attached" => srv.clients.iter().filter(|c| c.attached == self.sid && self.sid.is_some()).count().to_string(),
            "session_created" => sess?.created.to_string(),
            "session_activity" => sess?.activity.to_string(),
            "session_path" => sess?.cwd.clone(),
            "session_alerts" => String::new(),
            "window_index" => self.idx.or_else(|| sess.and_then(|s| s.index_of(self.wid?)))?.to_string(),
            "window_name" => srv.window_name(self.wid?),
            "window_id" => format!("@{}", self.wid?),
            "window_active" => b(sess.map(|s| Some(s.active) == self.wid).unwrap_or(false)),
            "window_panes" => win?.layout.panes().len().to_string(),
            "window_width" => win?.sx.to_string(),
            "window_height" => win?.sy.to_string(),
            "window_zoomed_flag" => b(win?.zoomed),
            "window_bell_flag" => b(win?.bell),
            "window_activity_flag" => b(win?.activity),
            "window_last_flag" => b(sess.map(|s| s.last == self.wid).unwrap_or(false)),
            "window_layout" => win?.layout.describe(),
            "window_flags" => {
                let s = sess?;
                let w = win?;
                let mut f = String::new();
                if Some(s.active) == self.wid {
                    f.push('*');
                } else if s.last == self.wid {
                    f.push('-');
                }
                if w.activity && srv.opt(self.sid, self.wid, "monitor-activity") == "on" {
                    f.push('#');
                }
                if w.bell {
                    f.push('!');
                }
                if w.zoomed {
                    f.push('Z');
                }
                f
            }
            "window_bigger" | "window_offset_x" | "window_offset_y" => "0".into(),
            "pane_index" => {
                let w = win?;
                let base = srv.gnum("pane-base-index", 0).max(0) as usize;
                (w.layout.panes().iter().position(|p| Some(*p) == self.pid)? + base).to_string()
            }
            "pane_id" => format!("%{}", self.pid?),
            "pane_title" => {
                let p = pane?;
                if p.meta.title.is_empty() { util::hostname() } else { p.meta.title.clone() }
            }
            "pane_current_path" => pane?.meta.cwd.clone(),
            "pane_current_command" => pane?.current_command(),
            "pane_start_command" => pane?.meta.argv.join(" "),
            "pane_pid" => pane?.child_pid.to_string(),
            "pane_keeper_pid" => pane?.keeper_pid.to_string(),
            "pane_active" => b(win.map(|w| Some(w.active) == self.pid).unwrap_or(false)),
            "pane_dead" => b(pane?.exited.is_some()),
            "pane_dead_status" => pane?.exited.map(pty::exit_code).map(|c| c.to_string()).unwrap_or_default(),
            "pane_in_mode" => b(pane?.in_copy),
            "pane_mode" => (if pane?.in_copy { "copy-mode" } else { "" }).to_string(),
            "pane_width" => self.rect().map(|r| r.w as usize).unwrap_or(pane?.mirror.cols).to_string(),
            "pane_height" => self.rect().map(|r| r.h as usize).unwrap_or(pane?.mirror.rows).to_string(),
            "pane_left" => self.rect()?.x.to_string(),
            "pane_top" => self.rect()?.y.to_string(),
            "pane_right" => {
                let r = self.rect()?;
                (r.x + r.w).saturating_sub(1).to_string()
            }
            "pane_bottom" => {
                let r = self.rect()?;
                (r.y + r.h).saturating_sub(1).to_string()
            }
            "pane_at_left" => b(self.rect()?.x == 0),
            "pane_at_top" => b(self.rect()?.y == 0),
            "pane_at_right" => {
                let r = self.rect()?;
                b(r.x + r.w >= win?.sx)
            }
            "pane_at_bottom" => {
                let r = self.rect()?;
                b(r.y + r.h >= win?.sy)
            }
            "pane_synchronized" => b(srv.opt(self.sid, self.wid, "synchronize-panes") == "on"),
            "pane_tty" => String::new(),
            "cursor_x" => pane?.mirror.cx.to_string(),
            "cursor_y" => pane?.mirror.cy.to_string(),
            "alternate_on" => b(pane?.mirror.modes.alt_screen),
            "mouse_any_flag" => b(pane?.mirror.modes.mouse != 0),
            "cursor_flag" => b(pane?.mirror.cursor_visible),
            "insert_flag" => b(pane?.mirror.modes.insert),
            "keypad_cursor_flag" => b(pane?.mirror.modes.app_cursor),
            "keypad_flag" => b(pane?.mirror.modes.app_keypad),
            "origin_flag" => b(pane?.mirror.modes.origin),
            "wrap_flag" => b(pane?.mirror.modes.autowrap),
            "history_limit" => srv.gopt("history-limit"),
            "history_size" | "history_bytes" => "0".into(),
            "pane_recoveries" => pane?.recoveries.to_string(),
            "agent" | "pane_agent" => pane?.agent.agent.as_ref().map(|k| k.def.name.clone()).unwrap_or_default(),
            "agent_state" => {
                let p = pane?;
                if p.agent.agent.is_some() { p.agent.state.label().to_lowercase() } else { String::new() }
            }
            "agent_summary" => pane?.agent.summary.clone(),
            "amux_agents" => render::agent_badge(srv),
            "amux_plugins" => srv.plugins.status_text(),
            "client_name" => format!("client{}", self.client?.id),
            "client_width" => self.client?.cols.to_string(),
            "client_height" => self.client?.rows.to_string(),
            "client_session" => sess?.name.clone(),
            "client_prefix" => b(self.client?.is_prefix()),
            "client_key_table" => self.client?.key_table.clone(),
            "client_termname" => self.client?.term.clone(),
            "client_tty" => format!("client{}", self.client?.id),
            "client_pid" => self.client?.pid.to_string(),
            "host" => util::hostname(),
            "host_short" => util::hostname().split('.').next().unwrap_or("").to_string(),
            "pid" => std::process::id().to_string(),
            "version" => env!("CARGO_PKG_VERSION").to_string(),
            "start_time" => srv.start_time.to_string(),
            "socket_path" => srv.paths.server_sock().to_string_lossy().into_owned(),
            "server_restarts" => srv.restarts.to_string(),
            _ => return None,
        };
        Some(v)
    }

    fn shell(&self, cmd: &str) -> String {
        let mut cache = self.srv.shell_cache.borrow_mut();
        let iv = Duration::from_secs(self.srv.gnum("status-interval", 15).max(1) as u64);
        if let Some((v, t)) = cache.get(cmd)
            && t.elapsed() < iv
        {
            return v.clone();
        }
        // Run synchronously with a short timeout (status-line commands are tiny).
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .env("PATH", shim_path(self.srv))
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).lines().next().unwrap_or("").to_string())
            .unwrap_or_default();
        cache.insert(cmd.to_string(), (out.clone(), Instant::now()));
        out
    }
}

fn shim_path(srv: &Server) -> String {
    format!("{}:{}", srv.paths.shim_dir().display(), std::env::var("PATH").unwrap_or_default())
}

fn parse_dir_flags(a: &Args) -> Option<Dir> {
    if a.has('L') {
        Some(Dir::L)
    } else if a.has('R') {
        Some(Dir::R)
    } else if a.has('U') {
        Some(Dir::U)
    } else if a.has('D') {
        Some(Dir::D)
    } else {
        None
    }
}

const PRESETS: [&str; 5] = ["even-horizontal", "even-vertical", "main-horizontal", "main-vertical", "tiled"];

impl Server {
    // ----- queue -------------------------------------------------------

    pub fn run_queue(&mut self, cid: u32) {
        let mut guard = 0;
        loop {
            guard += 1;
            if guard > 10_000 {
                break;
            }
            let Some(i) = self.client_idx(cid) else { return };
            if self.clients[i].waiting.is_some() || self.clients[i].prompt.is_some() && self.clients[i].cli && self.clients[i].attached.is_none() {
                return;
            }
            let Some(argv) = self.clients[i].queue.pop_front() else { break };
            let r = self.exec(cid, argv);
            if let Some(i) = self.client_idx(cid) {
                self.clients[i].settle_retry = false;
            }
            match r {
                Ok(Flow::Next) => {}
                Ok(Flow::Wait) => return,
                Err(e) => {
                    self.command_failed(cid, &e);
                    break;
                }
            }
        }
        if self.state_dirty {
            self.save_state();
        }
        if let Some(i) = self.client_idx(cid) {
            let c = &mut self.clients[i];
            if c.cli && c.attached.is_none() && c.waiting.is_none() && c.queue.is_empty() && !c.closing && !c.events_json {
                if c.plugin.is_some() {
                    // A plugin's `run` request finished; the client stays.
                    self.plugin_run_finished(cid);
                } else {
                    let mut e = Enc::new();
                    e.i32(c.exit_code).str("");
                    c.out(s2c::EXIT, &e.buf);
                    c.closing = true;
                }
            }
        }
    }

    /// Print command output: stdout for CLI clients, a view for attached ones.
    pub fn output(&mut self, cid: u32, title: &str, lines: Vec<String>) {
        let Some(i) = self.client_idx(cid) else { return };
        let c = &mut self.clients[i];
        if c.attached.is_some() {
            if lines.len() == 1 {
                let l = lines[0].clone();
                self.show_message(cid, &l);
            } else if !lines.is_empty() {
                c.overlay = Some(Overlay::View { title: title.to_string(), lines, top: 0 });
                c.dirty = true;
            }
        } else {
            let mut s = lines.join("\n");
            if !s.is_empty() {
                s.push('\n');
                c.out(s2c::STDOUT, s.as_bytes());
            }
        }
    }

    pub fn exec(&mut self, cid: u32, argv: Vec<String>) -> R {
        if argv.is_empty() {
            return Ok(Flow::Next);
        }
        self.status_gen += 1;
        // NAME=value lines set the global environment.
        if argv.len() == 1
            && let Some((k, v)) = argv[0].split_once('=')
            && !k.is_empty()
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            self.env.retain(|(a, _)| a != k);
            self.env.push((k.to_string(), v.to_string()));
            return Ok(Flow::Next);
        }
        let Some(cmd) = crate::cmd::lookup_command(&argv[0]) else {
            return self.exec_unknown(cid, argv);
        };
        let mut argv = argv;
        if self.client_idx(cid).is_some_and(|i| self.clients[i].plugin.is_some()) {
            // Plugins have no terminal: never attach their internal client.
            match cmd {
                "attach-session" | "suspend-client" | "lock-server" | "lock-session" | "lock-client" => {
                    return Err(format!("{}: not available to plugins", cmd));
                }
                "new-session" if !argv.iter().any(|a| a == "-d") => argv.insert(1, "-d".into()),
                _ => {}
            }
        }
        let rest = &argv[1..];
        let r = self.dispatch(cid, cmd, rest);
        if r.is_ok() {
            self.mark_dirty_after(cmd);
        }
        r
    }

    fn mark_dirty_after(&mut self, cmd: &str) {
        if !matches!(
            cmd,
            "list-sessions"
                | "list-windows"
                | "list-panes"
                | "list-keys"
                | "list-buffers"
                | "list-clients"
                | "list-commands"
                | "show-options"
                | "show-window-options"
                | "show-buffer"
                | "show-messages"
                | "show-environment"
                | "display-message"
                | "display-panes"
                | "capture-pane"
                | "has-session"
                | "send-keys"
                | "send-prefix"
                | "paste-buffer"
                | "set-buffer"
                | "delete-buffer"
                | "copy-mode"
                | "clear-history"
                | "refresh-client"
                | "wait-for"
                | "agents"
                | "agent-view"
                | "events"
                | "await"
                | "server-info"
                | "plugin"
        ) {
            self.state_dirty = true;
        }
    }

    // ----- target resolution ------------------------------------------

    pub fn current(&self, cid: u32) -> (Option<u32>, Option<u32>, Option<u32>) {
        let c = self.client_idx(cid).map(|i| &self.clients[i]);
        if let Some(c) = c {
            if let Some(sid) = c.attached
                && let Some(s) = self.sessions.iter().find(|s| s.id == sid)
            {
                let wid = s.active;
                return (Some(sid), Some(wid), self.windows.get(&wid).map(|w| w.active));
            }
            if let Some(p) = c.caller_pane
                && self.panes.contains_key(&p)
                && let Some(w) = self.window_of_pane(p)
            {
                return (self.session_of_window(w), Some(w), Some(p));
            }
        }
        match self.sessions.iter().max_by_key(|s| (s.activity, s.id)) {
            Some(s) => (Some(s.id), Some(s.active), self.windows.get(&s.active).map(|w| w.active)),
            None => (None, None, None),
        }
    }

    pub fn find_session(&self, cid: u32, t: &str) -> Result<u32, String> {
        if t.is_empty() {
            return self.current(cid).0.ok_or_else(|| "no current session".to_string());
        }
        if let Some(id) = t.strip_prefix('$').and_then(|s| s.parse::<u32>().ok()) {
            return self.sessions.iter().find(|s| s.id == id).map(|s| s.id).ok_or_else(|| format!("can't find session: {}", t));
        }
        if let Some(n) = t.strip_prefix('=') {
            return self.sessions.iter().find(|s| s.name == n).map(|s| s.id).ok_or_else(|| format!("can't find session: {}", n));
        }
        if let Some(s) = self.sessions.iter().find(|s| s.name == t) {
            return Ok(s.id);
        }
        let pre: Vec<&Session> = self.sessions.iter().filter(|s| s.name.starts_with(t)).collect();
        if pre.len() == 1 {
            return Ok(pre[0].id);
        }
        if pre.len() > 1 {
            return Err(format!("ambiguous session: {}", t));
        }
        Err(format!("can't find session: {}", t))
    }

    fn find_window_in(&self, sid: u32, t: &str) -> Result<u32, String> {
        let s = self.sessions.iter().find(|s| s.id == sid).ok_or("no session")?;
        if t.is_empty() {
            return Ok(s.active);
        }
        if let Some(id) = t.strip_prefix('@').and_then(|x| x.parse::<u32>().ok()) {
            return if self.windows.contains_key(&id) { Ok(id) } else { Err(format!("can't find window: {}", t)) };
        }
        let pos = s.windows.iter().position(|(_, w)| *w == s.active).unwrap_or(0);
        let n = s.windows.len();
        let off = |t: &str| t[1..].parse::<usize>().unwrap_or(1);
        match t {
            "!" | "{last}" => return s.last.ok_or_else(|| "no last window".to_string()),
            "^" | "{start}" => return Ok(s.windows[0].1),
            "$" | "{end}" => return Ok(s.windows[n - 1].1),
            "{next}" => return Ok(s.windows[(pos + 1) % n].1),
            "{previous}" => return Ok(s.windows[(pos + n - 1) % n].1),
            _ => {}
        }
        if t.starts_with('+') {
            return Ok(s.windows[(pos + off(t)) % n].1);
        }
        if t.starts_with('-') && t[1..].chars().all(|c| c.is_ascii_digit()) {
            return Ok(s.windows[(pos + n * 8 - off(t) % n) % n].1);
        }
        let tt = t.strip_prefix('=').unwrap_or(t);
        if let Ok(idx) = tt.parse::<u32>() {
            return s.window_at(idx).ok_or_else(|| format!("can't find window: {}", t));
        }
        if let Some((_, w)) = s.windows.iter().find(|(_, w)| self.window_name(*w) == tt) {
            return Ok(*w);
        }
        let pre: Vec<u32> = s.windows.iter().filter(|(_, w)| self.window_name(*w).starts_with(tt)).map(|(_, w)| *w).collect();
        if pre.len() == 1 {
            return Ok(pre[0]);
        }
        Err(format!("can't find window: {}", t))
    }

    fn find_pane_in(&self, wid: u32, t: &str) -> Result<u32, String> {
        let w = self.windows.get(&wid).ok_or("no window")?;
        let panes = w.layout.panes();
        if t.is_empty() {
            return Ok(w.active);
        }
        if let Some(id) = t.strip_prefix('%').and_then(|x| x.parse::<u32>().ok()) {
            return if self.panes.contains_key(&id) { Ok(id) } else { Err(format!("can't find pane: {}", t)) };
        }
        let pos = panes.iter().position(|p| *p == w.active).unwrap_or(0);
        let n = panes.len();
        match t {
            "!" | "{last}" => return w.last.filter(|l| panes.contains(l)).ok_or_else(|| "no last pane".to_string()),
            "+" | "{next}" => return Ok(panes[(pos + 1) % n]),
            "-" | "{previous}" => return Ok(panes[(pos + n - 1) % n]),
            "{top}" | "{top-left}" => return Ok(panes[0]),
            "{bottom}" | "{bottom-right}" => return Ok(panes[n - 1]),
            "{up}" | "{up-of}" => return self.neighbour(wid, w.active, Dir::U).ok_or_else(|| "no pane".to_string()),
            "{down}" | "{down-of}" => return self.neighbour(wid, w.active, Dir::D).ok_or_else(|| "no pane".to_string()),
            "{left}" | "{left-of}" => return self.neighbour(wid, w.active, Dir::L).ok_or_else(|| "no pane".to_string()),
            "{right}" | "{right-of}" => return self.neighbour(wid, w.active, Dir::R).ok_or_else(|| "no pane".to_string()),
            _ => {}
        }
        if let Some(k) = t.strip_prefix('+').and_then(|x| x.parse::<usize>().ok()) {
            return Ok(panes[(pos + k) % n]);
        }
        if let Some(k) = t.strip_prefix('-').and_then(|x| x.parse::<usize>().ok()) {
            return Ok(panes[(pos + n * 8 - k % n) % n]);
        }
        if let Ok(idx) = t.parse::<usize>() {
            let base = self.gnum("pane-base-index", 0).max(0) as usize;
            return idx.checked_sub(base).and_then(|i| panes.get(i)).copied().ok_or_else(|| format!("can't find pane: {}", t));
        }
        Err(format!("can't find pane: {}", t))
    }

    /// Resolve `-t` for a window target: (session, window).
    pub fn target_window(&self, cid: u32, t: Option<&str>) -> Result<(u32, u32), String> {
        let t = t.unwrap_or("");
        if t.is_empty() {
            let (s, w, _) = self.current(cid);
            return Ok((s.ok_or("no current session")?, w.ok_or("no current window")?));
        }
        if t.starts_with('%') {
            let (s, w, _) = self.target_pane(cid, Some(t))?;
            return Ok((s, w));
        }
        if let Some(id) = t.strip_prefix('@').and_then(|x| x.split('.').next()).and_then(|x| x.parse::<u32>().ok()) {
            let s = self.session_of_window(id).ok_or_else(|| format!("can't find window: {}", t))?;
            return Ok((s, id));
        }
        if let Some((sp, wp)) = t.split_once(':') {
            let sid = self.find_session(cid, sp)?;
            let wp = wp.split('.').next().unwrap_or("");
            return Ok((sid, self.find_window_in(sid, wp)?));
        }
        let (cur, _, _) = self.current(cid);
        if let Some(sid) = cur
            && let Ok(w) = self.find_window_in(sid, t.split('.').next().unwrap_or(t))
        {
            return Ok((sid, w));
        }
        let sid = self.find_session(cid, t)?;
        let s = self.sessions.iter().find(|s| s.id == sid).unwrap();
        Ok((sid, s.active))
    }

    pub fn target_pane(&self, cid: u32, t: Option<&str>) -> Result<(u32, u32, u32), String> {
        let t = t.unwrap_or("");
        if t.is_empty() {
            let (s, w, p) = self.current(cid);
            return Ok((s.ok_or("no current session")?, w.ok_or("no current window")?, p.ok_or("no current pane")?));
        }
        if let Some(id) = t.strip_prefix('%').and_then(|x| x.parse::<u32>().ok()) {
            if !self.panes.contains_key(&id) {
                return Err(format!("can't find pane: {}", t));
            }
            let w = self.window_of_pane(id).ok_or_else(|| format!("can't find pane: {}", t))?;
            let s = self.session_of_window(w).ok_or("no session")?;
            return Ok((s, w, id));
        }
        // session:window.pane / window.pane / .pane
        let (swpart, ppart) = match t.rfind('.') {
            Some(i) if !t[..i].ends_with(':') || i > 0 => (&t[..i], Some(&t[i + 1..])),
            _ => (t, None),
        };
        if let Some(p) = t.strip_prefix('.') {
            let (s, w, _) = self.current(cid);
            let (s, w) = (s.ok_or("no session")?, w.ok_or("no window")?);
            return Ok((s, w, self.find_pane_in(w, p)?));
        }
        let (s, w) = self.target_window(cid, Some(swpart))?;
        let p = self.find_pane_in(w, ppart.unwrap_or(""))?;
        Ok((s, w, p))
    }

    pub fn neighbour(&self, wid: u32, pid: u32, dir: Dir) -> Option<u32> {
        let rects = self.pane_rects(wid);
        let r = rects.iter().find(|(p, _)| *p == pid)?.1;
        let pane = self.panes.get(&pid);
        let (cx, cy) = pane.map(|p| (r.x + p.mirror.cx as u16, r.y + p.mirror.cy as u16)).unwrap_or((r.x, r.y));
        let w = self.windows.get(&wid)?;
        let cands: Vec<(u32, Rect)> = rects
            .iter()
            .filter(|(p, o)| {
                *p != pid
                    && match dir {
                        Dir::L => (o.x + o.w + 1 == r.x || (r.x == 0 && o.x + o.w == w.sx)) && o.y < r.y + r.h && r.y < o.y + o.h,
                        Dir::R => (r.x + r.w + 1 == o.x || (r.x + r.w == w.sx && o.x == 0)) && o.y < r.y + r.h && r.y < o.y + o.h,
                        Dir::U => (o.y + o.h + 1 == r.y || (r.y == 0 && o.y + o.h == w.sy)) && o.x < r.x + r.w && r.x < o.x + o.w,
                        Dir::D => (r.y + r.h + 1 == o.y || (r.y + r.h == w.sy && o.y == 0)) && o.x < r.x + r.w && r.x < o.x + o.w,
                    }
            })
            .copied()
            .collect();
        if cands.is_empty() {
            return None;
        }
        // Prefer the previously active pane, then the one under the cursor line.
        if let Some(l) = w.last
            && cands.iter().any(|(p, _)| *p == l)
        {
            return Some(l);
        }
        let best = cands.iter().find(|(_, o)| match dir {
            Dir::L | Dir::R => cy >= o.y && cy < o.y + o.h,
            Dir::U | Dir::D => cx >= o.x && cx < o.x + o.w,
        });
        Some(best.unwrap_or(&cands[0]).0)
    }

    // ----- creation helpers -------------------------------------------

    fn session_size(&self, cid: u32, a: &Args) -> (u16, u16) {
        let detached = a.has('d');
        let (dw, dh) = {
            let ds = self.gopt("default-size");
            let (x, y) = ds.split_once('x').unwrap_or(("80", "24"));
            (x.parse().unwrap_or(80), y.parse().unwrap_or(24))
        };
        let scid = self
            .caller_pane_here(cid)
            .and_then(|p| self.window_of_pane(p))
            .and_then(|w| self.session_of_window(w))
            .and_then(|s| self.target_attached_client(cid, Some(s)))
            .unwrap_or(cid);
        let c = self.client_idx(scid).map(|i| &self.clients[i]);
        // Like tmux: an attached client's terminal (minus the status line)
        // decides the size; otherwise -x/-y or default-size, used as is.
        match c {
            Some(c) if !detached && (!c.term.is_empty() || c.attached.is_some()) && c.cols > 0 => {
                let status = if self.gopt("status") == "off" { 0 } else { 1 };
                (c.cols.max(2), c.rows.saturating_sub(status).max(1))
            }
            _ => {
                let w = a.get('x').and_then(|v| v.parse().ok()).unwrap_or(dw);
                let h = a.get('y').and_then(|v| v.parse().ok()).unwrap_or(dh);
                (w.max(2), h.max(1))
            }
        }
    }

    fn command_from(&self, pos: &[String]) -> Vec<String> {
        match pos.len() {
            0 => vec![],
            1 => vec![pos[0].clone()],
            _ => pos.to_vec(),
        }
    }

    /// Create a window (with one pane) in session `sid`.
    pub fn create_window(
        &mut self,
        sid: u32,
        idx: Option<u32>,
        name: Option<String>,
        cwd: String,
        argv: Vec<String>,
        size: (u16, u16),
    ) -> Result<(u32, u32), String> {
        let wid = self.next_window;
        self.next_window += 1;
        let pid = self.alloc_pane_id();
        let meta = PaneMeta { id: pid, argv: argv.clone(), cwd, created: util::now_unix(), ..Default::default() };
        self.spawn_keeper(meta, Some(sid), size, None, None, None).map_err(|e| format!("create pane failed: {}", e))?;
        let auto = name.is_none();
        let win = Window {
            id: wid,
            name: name.unwrap_or_else(|| {
                argv.first().map(|a| a.split_whitespace().next().unwrap_or(a).rsplit('/').next().unwrap_or(a).to_string()).unwrap_or_default()
            }),
            auto_name: auto,
            layout: Node::pane(pid, size.0),
            active: pid,
            last: None,
            zoomed: false,
            sx: size.0,
            sy: size.1,
            bell: false,
            activity: false,
        };
        self.windows.insert(wid, win);
        let base = self.gnum("base-index", 0).max(0) as u32;
        let s = self.sessions.iter_mut().find(|s| s.id == sid).ok_or("no session")?;
        let idx = match idx {
            Some(i) => i,
            None => s.next_free_index(base),
        };
        s.insert(idx, wid);
        self.mark_dirty();
        self.emit_event("pane-new", pid, "");
        Ok((wid, pid))
    }

    fn create_session(&mut self, cid: u32, a: &Args) -> Result<u32, String> {
        let name = match a.get('s') {
            Some(n) => {
                if n.contains(':') || n.contains('.') {
                    return Err(format!("invalid session name: {}", n));
                }
                if self.sessions.iter().any(|s| s.name == n) {
                    return Err(format!("duplicate session: {}", n));
                }
                n.to_string()
            }
            None => {
                let mut i = 0;
                while self.sessions.iter().any(|s| s.name == i.to_string()) {
                    i += 1;
                }
                i.to_string()
            }
        };
        let ccwd = self.client_idx(cid).map(|i| self.clients[i].cwd.clone()).unwrap_or_default();
        let cwd = a.get('c').map(util::expand_home).filter(|s| !s.is_empty()).unwrap_or(if ccwd.is_empty() { util::home() } else { ccwd });
        let size = self.session_size(cid, a);
        // Client environment updates (update-environment).
        if let Some(i) = self.client_idx(cid) {
            let keep: Vec<String> = self.gopt("update-environment").split_whitespace().map(|s| s.to_string()).collect();
            let upd: Vec<(String, String)> =
                self.clients[i].env.iter().filter(|(k, _)| keep.contains(k) || k == "COLORTERM" || k == "TERM_PROGRAM").cloned().collect();
            for (k, v) in upd {
                self.env.retain(|(x, _)| *x != k);
                self.env.push((k, v));
            }
        }
        let sid = self.next_session;
        self.next_session += 1;
        self.sessions.push(Session {
            id: sid,
            name,
            windows: vec![],
            active: 0,
            last: None,
            created: util::now_unix(),
            activity: util::now_unix(),
            cwd: cwd.clone(),
        });
        let argv = self.command_from(&a.pos);
        match self.create_window(sid, None, a.get('n').map(|s| s.to_string()), cwd, argv, size) {
            Ok((wid, _)) => {
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) {
                    s.active = wid;
                }
                Ok(sid)
            }
            Err(e) => {
                self.sessions.retain(|s| s.id != sid);
                Err(e)
            }
        }
    }

    /// Pane of this server from which unattached CLI client `cid` was invoked.
    fn caller_pane_here(&self, cid: u32) -> Option<u32> {
        let c = &self.clients[self.client_idx(cid)?];
        if c.attached.is_some() {
            return None;
        }
        let pid = c.caller_pane?;
        let pane = self.panes.get(&pid)?;
        let has_amux = c.env.iter().any(|(k, _)| k == "AMUX");
        if has_amux || (pane.child_pid != 0 && util::proc_stat(c.pid).is_some_and(|st| st.session == pane.child_pid)) { Some(pid) } else { None }
    }

    fn detach_other_clients(&mut self, sid: u32, keep: u32) {
        let others: Vec<u32> = self.clients.iter().filter(|c| c.attached == Some(sid) && c.id != keep).map(|c| c.id).collect();
        for o in others {
            self.detach_client(o, "[detached (from session)]");
        }
    }

    /// Resolve a session target (`name`, `$id`, `=name`, or `session:window.pane`/`@w`/`%p`),
    /// selecting the addressed window and pane when one is part of `t`.
    fn resolve_session_target(&mut self, cid: u32, t: Option<&str>) -> Result<u32, String> {
        let t = t.unwrap_or("");
        if t.is_empty() {
            return self.find_session(cid, "");
        }
        if let Some(s) = self.sessions.iter().find(|s| s.name == t) {
            return Ok(s.id);
        }
        if t.contains(':') || t.contains('.') || t.starts_with('%') || t.starts_with('@') {
            let (s, w, p) = self.target_pane(cid, Some(t))?;
            self.select_window(s, w);
            self.select_pane(p);
            Ok(s)
        } else {
            self.find_session(cid, t)
        }
    }

    /// Attach `cid` to `sid`, or switch the outer client when `cid` runs inside a pane here.
    fn attach_or_switch(&mut self, cid: u32, sid: u32, detach_others: bool) -> Result<(), String> {
        if let Some(pane) = self.caller_pane_here(cid) {
            let caller_sid = self.window_of_pane(pane).and_then(|w| self.session_of_window(w));
            let tcid =
                self.target_attached_client(cid, caller_sid).ok_or_else(|| "sessions should be nested with care, unset $AMUX_PANE to force".to_string())?;
            if detach_others {
                self.detach_other_clients(sid, tcid);
            }
            self.attach(tcid, sid);
            return Ok(());
        }
        if self.client_idx(cid).is_some_and(|i| self.clients[i].attached.is_none()) {
            if let Some(why) = self.loops_through_tmux(cid) {
                return Err(why);
            }
            if self.client_idx(cid).is_none_or(|i| self.clients[i].term.is_empty()) {
                return Err("not a terminal".into());
            }
        }
        if detach_others {
            self.detach_other_clients(sid, cid);
        }
        self.attach(cid, sid);
        Ok(())
    }

    pub fn attach(&mut self, cid: u32, sid: u32) {
        let mouse = self.opt(Some(sid), None, "mouse") == "on";
        let focus = self.gflag("focus-events");
        let banner = self.recovered_banner.take();
        let Some(i) = self.client_idx(cid) else { return };
        let c = &mut self.clients[i];
        let was = c.attached;
        if was != Some(sid) {
            c.last_session = was;
        }
        c.attached = Some(sid);
        c.overlay = None;
        c.repeat_until = None;
        c.full_redraw = true;
        c.dirty = true;
        c.activity = Instant::now();
        c.key_table = "root".into();
        if was != Some(sid) {
            c.out(s2c::ATTACHED, &Enc::new().u32(sid).buf);
        }
        if was.is_none() {
            c.out(s2c::OUTPUT, &render::terminal_setup(mouse, focus));
            c.outer_mouse = mouse;
            c.outer_focus = focus;
        }
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) {
            s.activity = util::now_unix();
        }
        self.update_window_sizes();
        if let Some(b) = banner {
            self.show_message(cid, &b);
        }
        if was.is_none() {
            self.client_event("client-attached", cid, Some(sid));
        } else if was != Some(sid) {
            self.client_event("client-session-changed", cid, Some(sid));
        }
        if let Some(p) = self.active_pane(sid)
            && let Some(pp) = self.panes.get_mut(&p)
        {
            pp.agent.seen();
        }
    }

    // ----- dispatch ------------------------------------------------------

    fn dispatch(&mut self, cid: u32, cmd: &str, rest: &[String]) -> R {
        macro_rules! args {
            ($spec:expr) => {
                Args::parse(rest, $spec).map_err(|e| format!("{}: {}", cmd, e))?
            };
        }
        match cmd {
            "new-session" => {
                let a = args!("AdDEPXc:e:F:n:s:t:x:y:");
                if a.has('A') {
                    let existing = match a.get('s') {
                        Some(n) => self.find_session(cid, &format!("={}", n)).ok(),
                        None => self.current(cid).0,
                    };
                    if let Some(sid) = existing {
                        if !a.has('d') {
                            self.attach_or_switch(cid, sid, a.has('D'))?;
                        }
                        return Ok(Flow::Next);
                    }
                }
                let cli_tty = self.client_idx(cid).is_some_and(|i| self.clients[i].cli && !self.clients[i].term.is_empty());
                let is_attached = self.client_idx(cid).is_some_and(|i| self.clients[i].attached.is_some());
                let want_attach = !a.has('d') && (cli_tty || is_attached);
                if want_attach
                    && !is_attached
                    && self.caller_pane_here(cid).is_none()
                    && let Some(why) = self.loops_through_tmux(cid)
                {
                    return Err(why);
                }
                let sid = self.create_session(cid, &a)?;
                if a.has('P') {
                    let f = a.get('F').unwrap_or("#{session_name}:").to_string();
                    let out = format::expand(&f, &Fv { srv: self, client: None, sid: Some(sid), wid: None, pid: None, idx: None });
                    self.output(cid, "", vec![out]);
                }
                if want_attach {
                    self.attach_or_switch(cid, sid, false)?;
                }
                Ok(Flow::Next)
            }
            "attach-session" => {
                let a = Args::parse_anywhere(rest, "dErxc:f:t:").map_err(|e| format!("{}: {}", cmd, e))?;
                if self.sessions.is_empty() {
                    return Err("no sessions".into());
                }
                if (a.get('t').is_some() && !a.pos.is_empty()) || a.pos.len() > 1 {
                    return Err("attach-session: too many arguments".into());
                }
                let t = a.get('t').or_else(|| a.pos.first().map(|s| s.as_str()));
                let sid = self.resolve_session_target(cid, t)?;
                if let Some(c) = a.get('c')
                    && let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid)
                {
                    s.cwd = util::expand_home(c);
                }
                self.attach_or_switch(cid, sid, a.has('d'))?;
                Ok(Flow::Next)
            }
            "detach-client" => {
                let a = args!("aPE:s:t:");
                if a.has('s') {
                    let sid = self.find_session(cid, a.get('s').unwrap())?;
                    let ids: Vec<u32> = self.clients.iter().filter(|c| c.attached == Some(sid)).map(|c| c.id).collect();
                    for i in ids {
                        self.detach_client(i, "[detached]");
                    }
                    return Ok(Flow::Next);
                }
                if a.has('a') {
                    let ids: Vec<u32> = self.clients.iter().filter(|c| c.attached.is_some() && c.id != cid).map(|c| c.id).collect();
                    for i in ids {
                        self.detach_client(i, "[detached]");
                    }
                    return Ok(Flow::Next);
                }
                if let Some(t) = self.target_attached_client(cid, None) {
                    let sname = self.clients[self.client_idx(t).unwrap()]
                        .attached
                        .and_then(|s| self.sessions.iter().find(|x| x.id == s))
                        .map(|s| s.name.clone())
                        .unwrap_or_default();
                    self.detach_client(t, &format!("[detached (from session {})]", sname));
                }
                Ok(Flow::Next)
            }
            "kill-session" => {
                let a = args!("aCt:");
                let sid = self.find_session(cid, a.get('t').unwrap_or(""))?;
                if a.has('a') {
                    let others: Vec<u32> = self.sessions.iter().filter(|s| s.id != sid).map(|s| s.id).collect();
                    for o in others {
                        self.destroy_session(o);
                    }
                } else {
                    self.destroy_session(sid);
                }
                self.redraw_all();
                Ok(Flow::Next)
            }
            "kill-server" => {
                let ids: Vec<u32> = self.panes.keys().copied().collect();
                for id in ids {
                    self.close_pane_keeper(id);
                    let _ = std::fs::remove_file(self.paths.snap_file(id));
                }
                self.panes.clear();
                self.windows.clear();
                self.sessions.clear();
                let ids: Vec<u32> = self.clients.iter().filter(|c| c.attached.is_some()).map(|c| c.id).collect();
                for c in ids {
                    self.detach_client(c, "[server exited]");
                }
                self.save_state();
                // Gone before the reply, so the next command starts a new server.
                let _ = std::fs::remove_file(self.paths.server_sock());
                self.quit = Some(0);
                Ok(Flow::Next)
            }
            "has-session" => {
                let a = args!("t:");
                self.find_session(cid, a.get('t').unwrap_or("")).map(|_| Flow::Next)
            }
            "list-sessions" => {
                let a = args!("F:f:");
                let f = a
                    .get('F')
                    .unwrap_or("#{session_name}: #{session_windows} windows (created #{t:session_created})#{?session_attached, (attached),}")
                    .to_string();
                let lines: Vec<String> = self
                    .sessions
                    .iter()
                    .map(|s| {
                        format::expand(
                            &f,
                            &Fv {
                                srv: self,
                                client: None,
                                sid: Some(s.id),
                                wid: Some(s.active),
                                pid: self.windows.get(&s.active).map(|w| w.active),
                                idx: None,
                            },
                        )
                    })
                    .collect();
                if lines.is_empty() && self.client_idx(cid).map(|i| self.clients[i].attached.is_none()).unwrap_or(true) {
                    return Err("no sessions".into());
                }
                self.output(cid, "sessions", lines);
                Ok(Flow::Next)
            }
            "rename-session" => {
                let a = args!("t:");
                let sid = self.find_session(cid, a.get('t').unwrap_or(""))?;
                let name = a.pos.first().ok_or("rename-session: missing name")?.clone();
                if name.contains(':') || name.contains('.') {
                    return Err(format!("invalid session name: {}", name));
                }
                if self.sessions.iter().any(|s| s.name == name && s.id != sid) {
                    return Err(format!("duplicate session: {}", name));
                }
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) {
                    s.name = name;
                }
                self.redraw_all();
                Ok(Flow::Next)
            }
            "switch-client" => {
                let a = Args::parse_anywhere(rest, "ElnprZc:t:T:").map_err(|e| format!("{}: {}", cmd, e))?;
                let tcid = match a.get('c') {
                    Some(name) => {
                        let want = name.strip_prefix("/dev/").unwrap_or(name);
                        let want_id = want.strip_prefix("client").unwrap_or(want).parse::<u32>().ok();
                        self.clients
                            .iter()
                            .find(|c| c.attached.is_some() && !c.closing && Some(c.id) == want_id)
                            .map(|c| c.id)
                            .ok_or_else(|| format!("can't find client: {}", name))?
                    }
                    None => {
                        let prefer = self.caller_pane_here(cid).and_then(|p| self.window_of_pane(p)).and_then(|w| self.session_of_window(w));
                        self.target_attached_client(cid, prefer).ok_or("no current client")?
                    }
                };
                if let Some(t) = a.get('T') {
                    if let Some(i) = self.client_idx(tcid) {
                        self.clients[i].key_table = t.to_string();
                        self.clients[i].dirty = true;
                    }
                    return Ok(Flow::Next);
                }
                if self.sessions.is_empty() {
                    return Err("no sessions".into());
                }
                if (a.get('t').is_some() && !a.pos.is_empty()) || a.pos.len() > 1 {
                    return Err("switch-client: too many arguments".into());
                }
                let Some(ti) = self.client_idx(tcid) else { return Ok(Flow::Next) };
                let cur = self.clients[ti].attached;
                let target = if a.has('l') {
                    self.clients[ti].last_session.filter(|s| self.sessions.iter().any(|x| x.id == *s)).ok_or("no last session")?
                } else if a.has('n') || a.has('p') {
                    let n = self.sessions.len();
                    let pos = self.sessions.iter().position(|s| Some(s.id) == cur).unwrap_or(0);
                    let k = if a.has('n') { (pos + 1) % n } else { (pos + n - 1) % n };
                    self.sessions[k].id
                } else {
                    let t = a.get('t').or_else(|| a.pos.first().map(|s| s.as_str())).ok_or("switch-client: missing target session")?;
                    self.resolve_session_target(cid, Some(t))?
                };
                self.attach(tcid, target);
                Ok(Flow::Next)
            }
            "new-window" => {
                let a = args!("abdkPSc:e:F:n:t:");
                let (sid, idx) = match a.get('t') {
                    Some(t) => {
                        let (sp, wp) = t.split_once(':').unwrap_or(("", t));
                        let sid = if sp.is_empty() && !t.contains(':') {
                            match t.parse::<u32>() {
                                Ok(_) => self.current(cid).0.ok_or("no session")?,
                                Err(_) => self.find_session(cid, t).or_else(|_| self.current(cid).0.ok_or("no session".to_string()))?,
                            }
                        } else {
                            self.find_session(cid, sp)?
                        };
                        (sid, wp.trim_start_matches('=').parse::<u32>().ok())
                    }
                    None => (self.current(cid).0.ok_or("no current session")?, None),
                };
                let idx = if a.has('a') {
                    let s = self.sessions.iter().find(|s| s.id == sid).ok_or("no session")?;
                    let cur = s.index_of(s.active).unwrap_or(0);
                    Some(s.next_free_index(cur + 1))
                } else {
                    idx
                };
                if let Some(i) = idx {
                    let s = self.sessions.iter().find(|s| s.id == sid).ok_or("no session")?;
                    if let Some(existing) = s.window_at(i) {
                        if a.has('k') {
                            self.destroy_window(existing);
                        } else {
                            return Err(format!("index in use: {}", i));
                        }
                    }
                }
                let (_, _, cur_pane) = self.current(cid);
                let cwd = self.resolve_cwd(cid, a.get('c'), cur_pane, Some(sid));
                let size = self.sessions.iter().find(|s| s.id == sid).and_then(|s| self.windows.get(&s.active)).map(|w| (w.sx, w.sy)).unwrap_or((80, 24));
                let argv = self.command_from(&a.pos);
                let (wid, pid) = self.create_window(sid, idx, a.get('n').map(|s| s.to_string()), cwd, argv, size)?;
                if !a.has('d') {
                    self.select_window(sid, wid);
                }
                if a.has('P') {
                    let f = a.get('F').unwrap_or("#{session_name}:#{window_index}.#{pane_index}").to_string();
                    let out = format::expand(&f, &Fv::for_pane(self, None, pid));
                    self.output(cid, "", vec![out]);
                }
                self.update_window_sizes();
                self.redraw_session(sid);
                Ok(Flow::Next)
            }
            "kill-window" => {
                let a = args!("at:");
                let (sid, wid) = self.target_window(cid, a.get('t'))?;
                if a.has('a') {
                    let others: Vec<u32> = self
                        .sessions
                        .iter()
                        .find(|s| s.id == sid)
                        .map(|s| s.windows.iter().map(|(_, w)| *w).filter(|w| *w != wid).collect())
                        .unwrap_or_default();
                    for o in others {
                        self.destroy_window(o);
                    }
                } else {
                    self.destroy_window(wid);
                }
                self.redraw_all();
                Ok(Flow::Next)
            }
            "select-window" => {
                let a = args!("lnpTt:");
                let (sid, wid) = if a.has('l') || a.has('n') || a.has('p') {
                    let (s, _, _) = self.current(cid);
                    let sid = match a.get('t') {
                        Some(t) => self.find_session(cid, t)?,
                        None => s.ok_or("no session")?,
                    };
                    let t = if a.has('l') {
                        "!"
                    } else if a.has('n') {
                        "+"
                    } else {
                        "-"
                    };
                    (sid, self.find_window_in(sid, t)?)
                } else {
                    self.target_window(cid, a.get('t'))?
                };
                let wid = if a.has('T') && self.sessions.iter().any(|s| s.id == sid && s.active == wid) {
                    self.sessions.iter().find(|s| s.id == sid).and_then(|s| s.last).unwrap_or(wid)
                } else {
                    wid
                };
                self.select_window(sid, wid);
                self.update_window_sizes();
                Ok(Flow::Next)
            }
            "next-window" | "previous-window" | "last-window" => {
                let a = args!("at:");
                let sid = self.find_session(cid, a.get('t').unwrap_or(""))?;
                let t = match cmd {
                    "next-window" => "+",
                    "previous-window" => "-",
                    _ => "!",
                };
                let wid = if a.has('a') {
                    let s = self.sessions.iter().find(|s| s.id == sid).ok_or("no session")?;
                    s.windows
                        .iter()
                        .map(|(_, w)| *w)
                        .find(|w| *w != s.active && self.windows.get(w).map(|x| x.bell || x.activity).unwrap_or(false))
                        .ok_or("no window with alerts")?
                } else {
                    self.find_window_in(sid, t)?
                };
                self.select_window(sid, wid);
                self.update_window_sizes();
                Ok(Flow::Next)
            }
            "rename-window" => {
                let a = args!("t:");
                let (_, wid) = self.target_window(cid, a.get('t'))?;
                let name = a.pos.first().ok_or("rename-window: missing name")?.clone();
                if let Some(w) = self.windows.get_mut(&wid) {
                    w.name = name;
                    w.auto_name = false;
                }
                self.redraw_all();
                Ok(Flow::Next)
            }
            "list-windows" => {
                let a = args!("aF:f:t:");
                let f = a
                    .get('F')
                    .unwrap_or(
                        "#{window_index}: #{window_name}#{window_flags} (#{window_panes} panes) [#{window_width}x#{window_height}]#{?window_active, (active),}",
                    )
                    .to_string();
                let sids: Vec<u32> =
                    if a.has('a') { self.sessions.iter().map(|s| s.id).collect() } else { vec![self.find_session(cid, a.get('t').unwrap_or(""))?] };
                let mut lines = Vec::new();
                for sid in sids {
                    let s = self.sessions.iter().find(|s| s.id == sid).unwrap();
                    for (idx, wid) in &s.windows {
                        let pid = self.windows.get(wid).map(|w| w.active);
                        let f2 = if a.has('a') && a.get('F').is_none() { format!("#{{session_name}}:{}", f) } else { f.clone() };
                        lines.push(format::expand(&f2, &Fv { srv: self, client: None, sid: Some(sid), wid: Some(*wid), pid, idx: Some(*idx) }));
                    }
                }
                self.output(cid, "windows", lines);
                Ok(Flow::Next)
            }
            "swap-window" => {
                let a = args!("ds:t:");
                let (s1, w1) = self.target_window(cid, a.get('s'))?;
                let (s2, w2) = self.target_window(cid, a.get('t'))?;
                if s1 != s2 {
                    return Err("swap-window across sessions is not supported".into());
                }
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id == s1) {
                    for e in s.windows.iter_mut() {
                        if e.1 == w1 {
                            e.1 = w2;
                        } else if e.1 == w2 {
                            e.1 = w1;
                        }
                    }
                }
                self.redraw_all();
                Ok(Flow::Next)
            }
            "move-window" => {
                let a = args!("abrdks:t:");
                if a.has('r') {
                    let sid = self.find_session(cid, a.get('t').unwrap_or(""))?;
                    let base = self.gnum("base-index", 0).max(0) as u32;
                    if let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) {
                        for (i, e) in s.windows.iter_mut().enumerate() {
                            e.0 = base + i as u32;
                        }
                    }
                    self.redraw_all();
                    return Ok(Flow::Next);
                }
                let (s1, w1) = self.target_window(cid, a.get('s'))?;
                let t = a.get('t').unwrap_or("");
                let (sp, wp) = t.split_once(':').unwrap_or(("", t));
                let s2 = if sp.is_empty() { s1 } else { self.find_session(cid, sp)? };
                let idx = wp.trim_start_matches('=').parse::<u32>().ok();
                for s in self.sessions.iter_mut() {
                    if s.id == s1 {
                        s.windows.retain(|(_, w)| *w != w1);
                        if s.active == w1 {
                            s.active = s.windows.first().map(|(_, w)| *w).unwrap_or(0);
                        }
                    }
                }
                let base = self.gnum("base-index", 0).max(0) as u32;
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id == s2) {
                    let i = match idx {
                        Some(i) if s.window_at(i).is_none() => i,
                        _ => s.next_free_index(base),
                    };
                    s.insert(i, w1);
                    if !a.has('d') {
                        s.active = w1;
                    }
                }
                let empty: Vec<u32> = self.sessions.iter().filter(|s| s.windows.is_empty()).map(|s| s.id).collect();
                for e in empty {
                    self.sessions.retain(|s| s.id != e);
                }
                self.update_window_sizes();
                self.redraw_all();
                Ok(Flow::Next)
            }
            "split-window" => {
                let a = args!("bdfhIvPZc:e:F:l:p:t:");
                let (sid, wid, pid) = self.target_pane(cid, a.get('t'))?;
                let horiz = a.has('h');
                let rect = self.rect_of(pid).unwrap_or_default();
                let total = if a.has('f') {
                    let w = self.windows.get(&wid).unwrap();
                    if horiz { w.sx } else { w.sy }
                } else if horiz {
                    rect.w
                } else {
                    rect.h
                };
                let size = if let Some(l) = a.get('l') {
                    match l.strip_suffix('%') {
                        Some(pct) => Some((total as u32 * pct.parse::<u32>().unwrap_or(50) / 100) as u16),
                        None => l.parse::<u16>().ok(),
                    }
                } else {
                    a.get('p').and_then(|p| p.parse::<u32>().ok()).map(|pct| (total as u32 * pct / 100) as u16)
                };
                let cwd = self.resolve_cwd(cid, a.get('c'), Some(pid), Some(sid));
                let new = self.alloc_pane_id();
                let (sx, sy) = self.windows.get(&wid).map(|w| (w.sx, w.sy)).unwrap_or((80, 24));
                {
                    let w = self.windows.get_mut(&wid).unwrap();
                    w.zoomed = false;
                    if a.has('f') {
                        let root = std::mem::replace(&mut w.layout, Node::pane(0, 1));
                        let total = if horiz { sx } else { sy };
                        let b = size.unwrap_or(total / 2).clamp(1, total.saturating_sub(2).max(1));
                        let mut old = root;
                        old.size = total.saturating_sub(b + 1).max(1);
                        let nn = Node::pane(new, b);
                        let children = if a.has('b') { vec![nn, old] } else { vec![old, nn] };
                        w.layout = Node { size: 1, kind: crate::model::Kind::Split { horiz, children } };
                    } else {
                        w.layout.split(pid, new, horiz, size, a.has('b'), sx, sy)?;
                    }
                    w.layout.fit(sx, sy);
                }
                let mut nr = Vec::new();
                self.windows.get(&wid).unwrap().layout.layout(Rect { x: 0, y: 0, w: sx, h: sy }, &mut nr);
                let nsize = nr.iter().find(|(p, _)| *p == new).map(|(_, r)| (r.w, r.h)).unwrap_or((sx, sy));
                let argv = self.command_from(&a.pos);
                let meta = PaneMeta { id: new, argv, cwd, created: util::now_unix(), ..Default::default() };
                if let Err(e) = self.spawn_keeper(meta, Some(sid), nsize, None, None, None) {
                    let w = self.windows.get_mut(&wid).unwrap();
                    w.layout.remove(new);
                    w.layout.fit(sx, sy);
                    return Err(format!("create pane failed: {}", e));
                }
                if !a.has('d') {
                    self.select_pane(new);
                }
                self.resize_window_panes(wid);
                self.emit_event("pane-new", new, "");
                if a.has('P') {
                    let f = a.get('F').unwrap_or("#{session_name}:#{window_index}.#{pane_index}").to_string();
                    let out = format::expand(&f, &Fv::for_pane(self, None, new));
                    self.output(cid, "", vec![out]);
                }
                self.redraw_window(wid);
                Ok(Flow::Next)
            }
            "kill-pane" => {
                let a = args!("at:");
                let (_, wid, pid) = self.target_pane(cid, a.get('t'))?;
                if a.has('a') {
                    let others: Vec<u32> = self.windows.get(&wid).map(|w| w.layout.panes().into_iter().filter(|p| *p != pid).collect()).unwrap_or_default();
                    for o in others {
                        self.destroy_pane(o);
                    }
                } else {
                    self.destroy_pane(pid);
                }
                Ok(Flow::Next)
            }
            "select-pane" => {
                let a = args!("DdegLlMmRUZT:t:");
                let (_, wid, pid) = self.target_pane(cid, a.get('t'))?;
                if let Some(title) = a.get('T') {
                    if let Some(p) = self.panes.get_mut(&pid) {
                        p.meta.title = title.to_string();
                    }
                    self.redraw_all();
                    return Ok(Flow::Next);
                }
                let target = if let Some(d) = parse_dir_flags(&a) {
                    let from = self.windows.get(&wid).map(|w| w.active).unwrap_or(pid);
                    match self.neighbour(wid, if a.get('t').is_some() { pid } else { from }, d) {
                        Some(p) => p,
                        None => return Ok(Flow::Next),
                    }
                } else if a.has('l') {
                    self.windows.get(&wid).and_then(|w| w.last).ok_or("no last pane")?
                } else {
                    pid
                };
                let zoomed = self.windows.get(&wid).map(|w| w.zoomed).unwrap_or(false);
                self.select_pane(target);
                if a.has('Z')
                    && zoomed
                    && let Some(w) = self.windows.get_mut(&wid)
                {
                    w.zoomed = true;
                }
                self.resize_window_panes(wid);
                Ok(Flow::Next)
            }
            "last-pane" => {
                let a = args!("deZt:");
                let (_, wid) = self.target_window(cid, a.get('t'))?;
                let l = self.windows.get(&wid).and_then(|w| w.last).ok_or("no last pane")?;
                self.select_pane(l);
                Ok(Flow::Next)
            }
            "resize-pane" => {
                let a = args!("DLMRTUZt:x:y:");
                let (_, wid, pid) = self.target_pane(cid, a.get('t'))?;
                if a.has('Z') {
                    if let Some(w) = self.windows.get_mut(&wid)
                        && (w.layout.panes().len() > 1 || w.zoomed)
                    {
                        w.zoomed = !w.zoomed;
                        w.active = pid;
                    }
                    self.resize_window_panes(wid);
                    self.redraw_window(wid);
                    return Ok(Flow::Next);
                }
                let n: u16 = a.pos.first().and_then(|s| s.parse().ok()).unwrap_or(1);
                if let Some(d) = parse_dir_flags(&a) {
                    self.resize_pane(pid, d, n);
                }
                let r = self.rect_of(pid).unwrap_or_default();
                if let Some(x) = a.get('x') {
                    let tw = self.parse_size(x, self.windows.get(&wid).map(|w| w.sx).unwrap_or(80));
                    if tw > r.w {
                        self.resize_pane(pid, Dir::R, tw - r.w);
                    } else if tw < r.w {
                        self.resize_pane(pid, Dir::L, r.w - tw);
                    }
                }
                if let Some(y) = a.get('y') {
                    let th = self.parse_size(y, self.windows.get(&wid).map(|w| w.sy).unwrap_or(24));
                    if th > r.h {
                        self.resize_pane(pid, Dir::D, th - r.h);
                    } else if th < r.h {
                        self.resize_pane(pid, Dir::U, r.h - th);
                    }
                }
                Ok(Flow::Next)
            }
            "swap-pane" => {
                let a = args!("dDUZs:t:");
                let (_, wid, dst) = self.target_pane(cid, a.get('t'))?;
                let src = if a.has('U') || a.has('D') {
                    let w = self.windows.get(&wid).ok_or("no window")?;
                    let panes = w.layout.panes();
                    let pos = panes.iter().position(|p| *p == dst).unwrap_or(0);
                    let n = panes.len();
                    panes[if a.has('U') { (pos + n - 1) % n } else { (pos + 1) % n }]
                } else {
                    self.target_pane(cid, a.get('s').or(Some("{last}"))).map(|x| x.2)?
                };
                let ws = self.window_of_pane(src).ok_or("no window")?;
                if ws != wid {
                    // swap across windows: exchange ids in both layouts
                    self.windows.get_mut(&ws).unwrap().layout.replace(src, u32::MAX);
                    self.windows.get_mut(&wid).unwrap().layout.replace(dst, src);
                    self.windows.get_mut(&ws).unwrap().layout.replace(u32::MAX, dst);
                    for w in [ws, wid] {
                        let ww = self.windows.get_mut(&w).unwrap();
                        if ww.active == src {
                            ww.active = dst;
                        } else if ww.active == dst {
                            ww.active = src;
                        }
                    }
                } else {
                    self.windows.get_mut(&wid).unwrap().layout.swap(src, dst);
                }
                self.resize_window_panes(wid);
                self.resize_window_panes(ws);
                if !a.has('d') {
                    self.select_pane(src);
                }
                self.redraw_all();
                Ok(Flow::Next)
            }
            "rotate-window" => {
                let a = args!("DUZt:");
                let (_, wid) = self.target_window(cid, a.get('t'))?;
                let w = self.windows.get_mut(&wid).ok_or("no window")?;
                let mut panes = w.layout.panes();
                if panes.len() > 1 {
                    let orig = panes.clone();
                    if a.has('D') {
                        panes.rotate_left(1);
                    } else {
                        panes.rotate_right(1);
                    }
                    for &o in &orig {
                        w.layout.replace(o, u32::MAX - o);
                    }
                    for (o, n) in orig.iter().zip(panes.iter()) {
                        w.layout.replace(u32::MAX - *o, *n);
                    }
                }
                self.resize_window_panes(wid);
                self.redraw_window(wid);
                Ok(Flow::Next)
            }
            "break-pane" => {
                let a = args!("abdPF:n:s:t:");
                let (sid, wid, pid) = self.target_pane(cid, a.get('s').or(a.get('t').filter(|t| t.starts_with('%'))))?;
                let w = self.windows.get(&wid).ok_or("no window")?;
                if w.layout.panes().len() == 1 {
                    return Err("can't break with only one pane".into());
                }
                let (sx, sy) = (w.sx, w.sy);
                {
                    let w = self.windows.get_mut(&wid).unwrap();
                    w.layout.remove(pid);
                    if w.active == pid {
                        w.active = w.layout.panes()[0];
                    }
                    w.layout.fit(sx, sy);
                }
                self.resize_window_panes(wid);
                let nw = self.next_window;
                self.next_window += 1;
                let name = a.get('n').map(|s| s.to_string());
                self.windows.insert(
                    nw,
                    Window {
                        id: nw,
                        name: name.clone().unwrap_or_default(),
                        auto_name: name.is_none(),
                        layout: Node::pane(pid, sx),
                        active: pid,
                        last: None,
                        zoomed: false,
                        sx,
                        sy,
                        bell: false,
                        activity: false,
                    },
                );
                let base = self.gnum("base-index", 0).max(0) as u32;
                let s = self.sessions.iter_mut().find(|s| s.id == sid).unwrap();
                let idx = s.next_free_index(base);
                s.insert(idx, nw);
                if !a.has('d') {
                    self.select_window(sid, nw);
                }
                self.resize_window_panes(nw);
                self.redraw_all();
                Ok(Flow::Next)
            }
            "join-pane" | "move-pane" => {
                let a = args!("bdfhvl:p:s:t:");
                let (_, ws, src) = self.target_pane(cid, Some(a.get('s').unwrap_or("{last}")))?;
                let (sid, wd, dst) = self.target_pane(cid, a.get('t'))?;
                if src == dst {
                    return Err("source and target panes must be different".into());
                }
                let horiz = a.has('h');
                // detach src from its window
                {
                    let w = self.windows.get_mut(&ws).unwrap();
                    let (sx, sy) = (w.sx, w.sy);
                    if !w.layout.remove(src) {
                        self.windows.remove(&ws);
                        for s in &mut self.sessions {
                            s.windows.retain(|(_, w)| *w != ws);
                            if s.active == ws {
                                s.active = s.windows.first().map(|(_, w)| *w).unwrap_or(0);
                            }
                        }
                    } else {
                        if w.active == src {
                            w.active = w.layout.panes()[0];
                        }
                        w.layout.fit(sx, sy);
                    }
                }
                if self.windows.contains_key(&ws) {
                    self.resize_window_panes(ws);
                }
                let (sx, sy) = self.windows.get(&wd).map(|w| (w.sx, w.sy)).unwrap_or((80, 24));
                let size = a.get('l').and_then(|l| l.parse::<u16>().ok());
                self.windows.get_mut(&wd).unwrap().layout.split(dst, src, horiz, size, a.has('b'), sx, sy)?;
                self.windows.get_mut(&wd).unwrap().layout.fit(sx, sy);
                if !a.has('d') {
                    self.select_window(sid, wd);
                    self.select_pane(src);
                }
                self.resize_window_panes(wd);
                self.sessions.retain(|s| !s.windows.is_empty());
                self.redraw_all();
                Ok(Flow::Next)
            }
            "list-panes" => {
                let a = args!("asF:f:t:");
                let f = a
                    .get('F')
                    .unwrap_or(
                        "#{pane_index}: [#{pane_width}x#{pane_height}] #{pane_current_command} #{pane_id}#{?pane_active, (active),}#{?pane_dead, (dead),}",
                    )
                    .to_string();
                let mut targets = Vec::new();
                if a.has('a') {
                    for s in &self.sessions {
                        for (_, w) in &s.windows {
                            targets.push(*w);
                        }
                    }
                } else if a.has('s') {
                    let sid = self.find_session(cid, a.get('t').unwrap_or(""))?;
                    targets.extend(self.sessions.iter().find(|s| s.id == sid).unwrap().windows.iter().map(|(_, w)| *w));
                } else {
                    targets.push(self.target_window(cid, a.get('t'))?.1);
                }
                let mut lines = Vec::new();
                for wid in targets {
                    let panes = self.windows.get(&wid).map(|w| w.layout.panes()).unwrap_or_default();
                    for p in panes {
                        let f2 = if a.has('a') && a.get('F').is_none() { format!("#{{session_name}}:#{{window_index}}.{}", f) } else { f.clone() };
                        lines.push(format::expand(&f2, &Fv::for_pane(self, None, p)));
                    }
                }
                self.output(cid, "panes", lines);
                Ok(Flow::Next)
            }
            "display-panes" => {
                let a = args!("bNd:t:");
                let ms = a.get('d').and_then(|d| d.parse::<u64>().ok()).unwrap_or(self.gnum("display-panes-time", 1000).max(1) as u64);
                if let Some(i) = self.client_idx(cid) {
                    self.clients[i].overlay = Some(Overlay::Panes { until: Instant::now() + Duration::from_millis(ms) });
                    self.clients[i].dirty = true;
                }
                Ok(Flow::Next)
            }
            "respawn-pane" | "respawn-window" => {
                let a = args!("kc:e:t:");
                let ids: Vec<u32> = if cmd == "respawn-window" {
                    let (_, wid) = self.target_window(cid, a.get('t'))?;
                    self.windows.get(&wid).map(|w| w.layout.panes()).unwrap_or_default()
                } else {
                    vec![self.target_pane(cid, a.get('t'))?.2]
                };
                for pid in ids {
                    let (dead, meta, size) = {
                        let p = self.panes.get(&pid).ok_or("no pane")?;
                        (p.exited.is_some(), p.meta.clone(), p.size)
                    };
                    if !dead && !a.has('k') {
                        return Err(format!("pane %{} still active", pid));
                    }
                    self.close_pane_keeper(pid);
                    let argv = if a.pos.is_empty() { meta.argv.clone() } else { self.command_from(&a.pos) };
                    let cwd = a.get('c').map(util::expand_home).unwrap_or(meta.cwd.clone());
                    let sid = self.window_of_pane(pid).and_then(|w| self.session_of_window(w));
                    self.spawn_keeper(PaneMeta { argv, cwd, ..meta }, sid, size, None, None, None).map_err(|e| e.to_string())?;
                    if let Some(w) = self.window_of_pane(pid) {
                        self.resize_window_panes(w);
                    }
                }
                self.redraw_all();
                Ok(Flow::Next)
            }
            "capture-pane" | "capture" => {
                let a = args!("aCeJNpPqb:E:S:t:");
                let (_, _, pid) = self.target_pane(cid, a.get('t'))?;
                let parse = |v: Option<&str>, dflt: i32| -> i32 {
                    match v {
                        Some("-") => {
                            if dflt <= 0 {
                                i32::MIN / 2
                            } else {
                                i32::MAX / 2
                            }
                        }
                        Some(s) => s.parse().unwrap_or(dflt),
                        None => dflt,
                    }
                };
                let start = parse(a.get('S'), 0);
                let end = parse(a.get('E'), i32::MAX / 2);
                let start = if a.get('S') == Some("-") { i32::MIN / 2 } else { start };
                let flags = (a.has('J') as u32) | ((a.has('N') as u32) << 1);
                let req = self.next_req;
                self.next_req += 1;
                let p = self.panes.get_mut(&pid).ok_or("no pane")?;
                p.send(s2k::CAPTURE, &Enc::new().u32(req).i32(start).i32(end).u32(flags).buf);
                let print = a.has('p') || cmd == "capture";
                let buffer = if print { None } else { Some(a.get('b').map(|s| s.to_string()).unwrap_or_default()) };
                if let Some(i) = self.client_idx(cid) {
                    let argv = std::iter::once(cmd.to_string()).chain(rest.iter().cloned()).collect();
                    let deadline = Instant::now() + Duration::from_secs(10);
                    self.clients[i].waiting = Some(Wait::Capture { pane: pid, req, buffer, print, argv, deadline });
                }
                Ok(Flow::Wait)
            }
            "clear-history" => {
                let a = args!("Ht:");
                let (_, _, pid) = self.target_pane(cid, a.get('t'))?;
                if let Some(p) = self.panes.get_mut(&pid) {
                    p.send(s2k::CLEAR_HISTORY, &[]);
                }
                Ok(Flow::Next)
            }
            "select-layout" | "next-layout" | "previous-layout" => {
                let a = args!("Enopt:");
                let (_, wid) = self.target_window(cid, a.get('t'))?;
                let w = self.windows.get(&wid).ok_or("no window")?;
                let panes = w.layout.panes();
                let name = if cmd == "next-layout" || a.has('n') || cmd == "previous-layout" || a.has('p') {
                    let last = self.opt(None, Some(wid), "@amux-layout");
                    let pos = PRESETS.iter().position(|p| *p == last).unwrap_or(PRESETS.len() - 1);
                    let fwd = cmd == "next-layout" || a.has('n');
                    PRESETS[if fwd { (pos + 1) % PRESETS.len() } else { (pos + PRESETS.len() - 1) % PRESETS.len() }].to_string()
                } else if a.has('E') {
                    "even".into()
                } else {
                    a.pos.first().cloned().unwrap_or_else(|| "tiled".into())
                };
                let mw = self.gnum("main-pane-width", 80) as u16;
                let mh = self.gnum("main-pane-height", 24) as u16;
                let (sx, sy) = (w.sx, w.sy);
                let newl = if name == "even" {
                    let mut l = w.layout.clone();
                    // equalize siblings at every level
                    fn eq(n: &mut Node) {
                        if let crate::model::Kind::Split { children, .. } = &mut n.kind {
                            for c in children.iter_mut() {
                                c.size = 1;
                                eq(c);
                            }
                        }
                    }
                    eq(&mut l);
                    l.fit(sx, sy);
                    Some(l)
                } else {
                    Node::preset(&name, &panes, sx, sy, mw.min(sx.saturating_sub(10)), mh.min(sy.saturating_sub(4)))
                };
                let Some(l) = newl else { return Err(format!("invalid layout: {}", name)) };
                let w = self.windows.get_mut(&wid).unwrap();
                w.layout = l;
                w.zoomed = false;
                self.options.window.entry(wid).or_default().insert("@amux-layout".into(), name);
                self.resize_window_panes(wid);
                self.redraw_window(wid);
                Ok(Flow::Next)
            }
            "send-keys" => {
                let a = args!("FHKlMRXN:t:");
                let (sid, _, pid) = self.target_pane(cid, a.get('t'))?;
                let n = a.get('N').and_then(|s| s.parse::<usize>().ok()).unwrap_or(1).max(1);
                if a.has('X') {
                    let in_copy = self.panes.get(&pid).map(|p| p.in_copy).unwrap_or(false);
                    if !in_copy {
                        return Err("not in a mode".into());
                    }
                    let args: Vec<&str> = a.pos.iter().map(|s| s.as_str()).collect();
                    for _ in 0..n {
                        self.copy_cmd(pid, &args);
                    }
                    return Ok(Flow::Next);
                }
                if a.has('R')
                    && let Some(p) = self.panes.get_mut(&pid)
                {
                    p.input(b"\x1bc");
                }
                let modes = self.panes.get(&pid).map(|p| p.mirror.modes).unwrap_or_default();
                let mut data = Vec::new();
                for arg in &a.pos {
                    if a.has('H') {
                        for h in arg.split_whitespace() {
                            if let Ok(v) = u8::from_str_radix(h, 16) {
                                data.push(v);
                            }
                        }
                    } else if a.has('l') {
                        data.extend_from_slice(arg.as_bytes());
                    } else if let Some(k) = parse_key_name(arg) {
                        data.extend(keys::encode(k, &modes));
                    } else {
                        data.extend_from_slice(arg.as_bytes());
                    }
                }
                let sync = self.window_of_pane(pid).map(|w| self.opt(Some(sid), Some(w), "synchronize-panes") == "on").unwrap_or(false);
                let targets = if sync && !a.has('t') {
                    self.window_of_pane(pid).and_then(|w| self.windows.get(&w)).map(|w| w.layout.panes()).unwrap_or_default()
                } else {
                    vec![pid]
                };
                for t in targets {
                    if let Some(p) = self.panes.get_mut(&t) {
                        for _ in 0..n {
                            p.input(&data);
                        }
                    }
                }
                Ok(Flow::Next)
            }
            "send-prefix" => {
                let a = args!("2t:");
                let (sid, _, pid) = self.target_pane(cid, a.get('t'))?;
                let k = parse_key_name(&self.opt(Some(sid), None, if a.has('2') { "prefix2" } else { "prefix" })).ok_or("no prefix")?;
                if let Some(p) = self.panes.get_mut(&pid) {
                    let b = keys::encode(k, &p.mirror.modes);
                    p.input(&b);
                }
                Ok(Flow::Next)
            }
            "copy-mode" => {
                let a = args!("eHMqus:t:");
                let (_, _, pid) = self.target_pane(cid, a.get('t'))?;
                if a.has('q') {
                    self.copy_cmd(pid, &["cancel"]);
                    return Ok(Flow::Next);
                }
                let in_copy = self.panes.get(&pid).map(|p| p.in_copy).unwrap_or(false);
                if !in_copy {
                    self.enter_copy(pid, a.has('e'), 0);
                }
                if a.has('u') {
                    self.copy_cmd(pid, &["page-up"]);
                }
                Ok(Flow::Next)
            }
            "paste-buffer" => {
                let a = args!("dprb:s:t:");
                let (_, _, pid) = self.target_pane(cid, a.get('t'))?;
                let Some((name, text)) = self.get_buffer(a.get('b')).cloned() else { return Ok(Flow::Next) };
                let sep = if a.has('r') { "\n".to_string() } else { a.get('s').unwrap_or("\r").to_string() };
                let data = text.replace('\n', &sep);
                if a.has('p') {
                    self.paste_to_pane(pid, data.as_bytes());
                } else if let Some(p) = self.panes.get_mut(&pid) {
                    p.input(data.as_bytes());
                }
                if a.has('d') {
                    self.buffers.retain(|(n, _)| *n != name);
                }
                Ok(Flow::Next)
            }
            "set-buffer" => {
                let a = args!("awb:n:t:");
                if let Some(nn) = a.get('n') {
                    let old = a.get('b').map(|s| s.to_string()).or_else(|| self.buffers.first().map(|b| b.0.clone())).ok_or("no buffer")?;
                    for bf in self.buffers.iter_mut() {
                        if bf.0 == old {
                            bf.0 = nn.to_string();
                        }
                    }
                    return Ok(Flow::Next);
                }
                let data = a.pos.first().cloned().unwrap_or_default();
                if a.has('a') {
                    let name = a.get('b').map(|s| s.to_string()).or_else(|| self.buffers.first().map(|b| b.0.clone()));
                    if let Some(n) = name
                        && let Some(bf) = self.buffers.iter_mut().find(|b| b.0 == n)
                    {
                        bf.1.push_str(&data);
                        return Ok(Flow::Next);
                    }
                }
                self.add_buffer(a.get('b').map(|s| s.to_string()), data.clone());
                if a.has('w') {
                    self.set_clipboard(&data);
                }
                Ok(Flow::Next)
            }
            "show-buffer" => {
                let a = args!("b:");
                let (_, t) = self.get_buffer(a.get('b')).ok_or("no buffers")?;
                let lines = t.lines().map(|l| l.to_string()).collect();
                self.output(cid, "buffer", lines);
                Ok(Flow::Next)
            }
            "list-buffers" | "choose-buffer" => {
                let lines: Vec<String> = self
                    .buffers
                    .iter()
                    .map(|(n, t)| {
                        let preview: String = t.chars().take(50).map(|c| if c.is_control() { ' ' } else { c }).collect();
                        format!("{}: {} bytes: \"{}\"", n, t.len(), preview)
                    })
                    .collect();
                self.output(cid, "buffers", lines);
                Ok(Flow::Next)
            }
            "delete-buffer" => {
                let a = args!("b:");
                match a.get('b') {
                    Some(n) => self.buffers.retain(|(x, _)| x != n),
                    None => {
                        if !self.buffers.is_empty() {
                            self.buffers.remove(0);
                        }
                    }
                }
                Ok(Flow::Next)
            }
            "load-buffer" => {
                let a = args!("wb:t:");
                let path = util::expand_home(a.pos.first().ok_or("load-buffer: missing path")?);
                let data = std::fs::read_to_string(&path).map_err(|e| format!("{}: {}", path, e))?;
                self.add_buffer(a.get('b').map(|s| s.to_string()), data);
                Ok(Flow::Next)
            }
            "save-buffer" => {
                let a = args!("ab:");
                let path = util::expand_home(a.pos.first().ok_or("save-buffer: missing path")?);
                let (_, t) = self.get_buffer(a.get('b')).ok_or("no buffers")?;
                let r = if a.has('a') {
                    use std::io::Write;
                    std::fs::OpenOptions::new().append(true).create(true).open(&path).and_then(|mut f| f.write_all(t.as_bytes()))
                } else {
                    std::fs::write(&path, t)
                };
                r.map_err(|e| format!("{}: {}", path, e))?;
                Ok(Flow::Next)
            }
            "set-option" | "set-window-option" => {
                let a = args!("aFgopqsuUwt:");
                self.set_option(cid, &a, cmd == "set-window-option")?;
                Ok(Flow::Next)
            }
            "show-options" | "show-window-options" => {
                let a = args!("AgHpqsvwt:");
                let mut lines = Vec::new();
                let mut keys: Vec<(String, String)> = self.options.global.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                if !a.has('g') {
                    let (s, w, _) = self.current(cid);
                    if let Some(sm) = s.and_then(|s| self.options.session.get(&s)) {
                        for (k, v) in sm {
                            keys.retain(|(x, _)| x != k);
                            keys.push((k.clone(), v.clone()));
                        }
                    }
                    if let Some(wm) = w.and_then(|w| self.options.window.get(&w)) {
                        for (k, v) in wm {
                            keys.retain(|(x, _)| x != k);
                            keys.push((k.clone(), v.clone()));
                        }
                    }
                }
                keys.sort();
                for (k, v) in keys {
                    if let Some(want) = a.pos.first()
                        && &k != want
                    {
                        continue;
                    }
                    if a.has('v') {
                        lines.push(v);
                    } else {
                        let q = if v.contains(' ') || v.is_empty() || v.contains('#') { format!("\"{}\"", v.replace('"', "\\\"")) } else { v };
                        lines.push(format!("{} {}", k, q));
                    }
                }
                if lines.is_empty() && !a.pos.is_empty() && !a.has('q') {
                    return Err(format!("invalid option: {}", a.pos[0]));
                }
                self.output(cid, "options", lines);
                Ok(Flow::Next)
            }
            "bind-key" => {
                let a = args!("nrN:T:");
                let key = a.pos.first().ok_or("bind-key: missing key")?;
                let k = parse_key_name(key).ok_or_else(|| format!("unknown key: {}", key))?;
                let table = if a.has('n') { "root".to_string() } else { a.get('T').unwrap_or("prefix").to_string() };
                let rest = &a.pos[1..];
                let cmds = if rest.len() == 1 && (rest[0].contains(' ') || rest[0].contains(';') || rest[0].contains('\n')) {
                    crate::cmd::parse_cmdlist(&rest[0], &|n| std::env::var(n).ok())?
                } else {
                    crate::cmd::split_semicolons(rest)
                };
                if cmds.is_empty() {
                    return Err("bind-key: missing command".into());
                }
                self.keys.bind(&table, k, cmds, a.has('r'), a.get('N').unwrap_or(""));
                Ok(Flow::Next)
            }
            "unbind-key" => {
                let a = args!("anqT:");
                let table = if a.has('n') { "root".to_string() } else { a.get('T').unwrap_or("prefix").to_string() };
                if a.has('a') {
                    if let Some(t) = self.keys.tables.get_mut(&table) {
                        t.clear();
                    }
                    return Ok(Flow::Next);
                }
                let key = a.pos.first().ok_or("unbind-key: missing key")?;
                match parse_key_name(key) {
                    Some(k) => self.keys.unbind(&table, k),
                    None if a.has('q') => {}
                    None => return Err(format!("unknown key: {}", key)),
                }
                Ok(Flow::Next)
            }
            "list-keys" => {
                let a = args!("1aNP:T:");
                let mut lines = Vec::new();
                let mut tables: Vec<&String> = self.keys.tables.keys().collect();
                tables.sort();
                for t in tables {
                    if let Some(want) = a.get('T')
                        && t != want
                    {
                        continue;
                    }
                    let mut entries: Vec<(String, &super::input::Binding)> = self.keys.tables[t].iter().map(|(k, b)| (keys::key_name(*k), b)).collect();
                    entries.sort_by(|x, y| x.0.cmp(&y.0));
                    for (k, b) in entries {
                        if let Some(want) = a.pos.first()
                            && &k != want
                        {
                            continue;
                        }
                        let cmds: Vec<String> = b.cmds.iter().map(|c| c.iter().map(|s| quote_arg(s)).collect::<Vec<_>>().join(" ")).collect();
                        lines.push(format!("bind-key {}-T {:<12} {:<10} {}", if b.repeat { "-r " } else { "   " }, t, quote_arg(&k), cmds.join(" \\; ")));
                    }
                }
                self.output(cid, "keys", lines);
                Ok(Flow::Next)
            }
            "source-file" => {
                let a = args!("Fnqv");
                let mut errs = Vec::new();
                for p in &a.pos {
                    let path = util::expand_home(p);
                    if !std::path::Path::new(&path).exists() {
                        if !a.has('q') {
                            errs.push(format!("{}: No such file or directory", path));
                        }
                        continue;
                    }
                    errs.extend(self.source_file(&path, Some(cid)));
                }
                if !errs.is_empty() {
                    return Err(errs.join("; "));
                }
                Ok(Flow::Next)
            }
            "run-shell" => {
                let a = args!("bCd:t:");
                let c = a.pos.first().cloned().ok_or("run-shell: missing command")?;
                if a.has('C') {
                    let cmds = crate::cmd::parse_cmdlist(&c, &|n| std::env::var(n).ok())?;
                    self.run_binding(cid, cmds);
                    return Ok(Flow::Next);
                }
                let c = format::expand(&c, &self.fv_current(cid));
                let id = self.start_job(&c, Some(cid), None, true, vec![], vec![]);
                if a.has('b') {
                    if let Some(j) = self.jobs.iter_mut().find(|j| j.id == id) {
                        j.client = None;
                    }
                    return Ok(Flow::Next);
                }
                if let Some(i) = self.client_idx(cid) {
                    self.clients[i].waiting = Some(Wait::Job { id });
                }
                Ok(Flow::Wait)
            }
            "if-shell" => {
                let a = args!("bFt:");
                let cond = a.pos.first().cloned().ok_or("if-shell: missing condition")?;
                let then = a.pos.get(1).cloned().unwrap_or_default();
                let els = a.pos.get(2).cloned().unwrap_or_default();
                let parse = |s: &str| crate::cmd::parse_cmdlist(s, &|n| std::env::var(n).ok()).unwrap_or_default();
                if a.has('F') {
                    let v = format::expand(&cond, &self.fv_current(cid));
                    let chosen = if !v.is_empty() && v != "0" { then } else { els };
                    let cmds = parse(&chosen);
                    self.run_binding(cid, cmds);
                    return Ok(Flow::Next);
                }
                let id = self.start_job(&cond, Some(cid), None, false, parse(&then), parse(&els));
                if a.has('b') {
                    return Ok(Flow::Next);
                }
                if let Some(i) = self.client_idx(cid) {
                    self.clients[i].waiting = Some(Wait::Job { id });
                }
                Ok(Flow::Wait)
            }
            "set-environment" => {
                let a = args!("Fhgrut:");
                let name = a.pos.first().ok_or("set-environment: missing name")?.clone();
                self.env.retain(|(k, _)| *k != name);
                if !a.has('u') && !a.has('r') {
                    let mut v = a.pos.get(1).cloned().unwrap_or_default();
                    if a.has('F') {
                        v = format::expand(&v, &self.fv_current(cid));
                    }
                    self.env.push((name, v));
                }
                Ok(Flow::Next)
            }
            "show-environment" => {
                let a = args!("hgst:");
                let mut lines: Vec<String> = self
                    .env
                    .iter()
                    .filter(|(k, _)| a.pos.first().map(|w| w == k).unwrap_or(true))
                    .map(|(k, v)| if a.has('s') { format!("{}=\"{}\"; export {};", k, v, k) } else { format!("{}={}", k, v) })
                    .collect();
                lines.sort();
                self.output(cid, "environment", lines);
                Ok(Flow::Next)
            }
            "display-message" => {
                let a = args!("aINpvc:d:F:t:");
                let fmt = a
                    .pos
                    .first()
                    .cloned()
                    .or_else(|| a.get('F').map(|s| s.to_string()))
                    .unwrap_or_else(|| "[#{session_name}] #{window_index}:#{window_name}, current pane #{pane_index} - (%H:%M %d-%b-%y)".into());
                let pid = match a.get('t') {
                    Some(t) => Some(self.target_pane(cid, Some(t))?.2),
                    None => self.current(cid).2,
                };
                let ci = self.client_idx(cid);
                let msg = {
                    let fv = match pid {
                        Some(p) => Fv::for_pane(self, ci.map(|i| &self.clients[i]), p),
                        None => Fv { srv: self, client: None, sid: None, wid: None, pid: None, idx: None },
                    };
                    format::strftime(&format::expand(&fmt, &fv))
                };
                if a.has('p') || ci.map(|i| self.clients[i].attached.is_none()).unwrap_or(true) {
                    if let Some(i) = ci {
                        self.clients[i].print(&msg);
                    }
                } else {
                    let d = a.get('d').and_then(|d| d.parse::<u64>().ok());
                    self.show_message(cid, &msg);
                    if let (Some(d), Some(i)) = (d, self.client_idx(cid))
                        && let Some(m) = self.clients[i].message.as_mut()
                    {
                        m.1 = Instant::now() + Duration::from_millis(d.max(1));
                    }
                }
                Ok(Flow::Next)
            }
            "command-prompt" => {
                let a = args!("1bFikNI:p:t:T:");
                if self.client_idx(cid).map(|i| self.clients[i].attached.is_none()).unwrap_or(true) {
                    return Ok(Flow::Next);
                }
                let label = a.get('p').map(|p| format!("{} ", p.split(',').next().unwrap_or(p))).unwrap_or_else(|| ":".into());
                let initial = a.get('I').map(|i| format::expand(i, &self.fv_current(cid))).unwrap_or_default();
                let template = a.pos.first().cloned().unwrap_or_default();
                self.open_prompt(cid, &label, &initial, &template, a.has('1'), false);
                Ok(Flow::Next)
            }
            "confirm-before" => {
                let a = args!("byp:t:");
                let c = a.pos.first().cloned().ok_or("confirm-before: missing command")?;
                let p = a.get('p').map(|p| format::expand(p, &self.fv_current(cid))).unwrap_or_else(|| format!("Confirm '{}'? (y/n)", c));
                self.open_prompt(cid, &format!("{} ", p), "", &c, false, true);
                Ok(Flow::Next)
            }
            "refresh-client" => {
                if let Some(i) = self.client_idx(cid) {
                    self.clients[i].full_redraw = true;
                    self.clients[i].dirty = true;
                }
                Ok(Flow::Next)
            }
            "suspend-client" => {
                if let Some(i) = self.client_idx(cid) {
                    let c = &mut self.clients[i];
                    c.out(s2c::OUTPUT, &render::terminal_reset());
                    c.out(s2c::SUSPEND, &[]);
                    c.full_redraw = true;
                    c.outer_mouse = false;
                }
                Ok(Flow::Next)
            }
            "list-commands" => {
                let mut lines: Vec<String> = crate::cmd::PUBLIC_COMMANDS.iter().map(|s| s.to_string()).collect();
                for (n, h, p) in self.plugins.commands() {
                    lines.push(if h.is_empty() { format!("{} (plugin {})", n, p) } else { format!("{} (plugin {}): {}", n, p, h) });
                }
                self.output(cid, "commands", lines);
                Ok(Flow::Next)
            }
            "show-messages" => {
                let lines: Vec<String> = self.messages.iter().cloned().collect();
                self.output(cid, "messages", lines);
                Ok(Flow::Next)
            }
            "choose-tree" | "choose-client" | "list-clients" => {
                if cmd == "list-clients" || cmd == "choose-client" {
                    let lines: Vec<String> = self
                        .clients
                        .iter()
                        .filter(|c| c.attached.is_some())
                        .map(|c| {
                            let s = c.attached.and_then(|s| self.sessions.iter().find(|x| x.id == s)).map(|s| s.name.clone()).unwrap_or_default();
                            format!("client{}: {} [{}x{} {}]", c.id, s, c.cols, c.rows, c.term)
                        })
                        .collect();
                    self.output(cid, "clients", lines);
                    return Ok(Flow::Next);
                }
                let a = args!("GNrswZF:f:K:O:t:");
                let prefer = self.caller_pane_here(cid).and_then(|p| self.window_of_pane(p)).and_then(|w| self.session_of_window(w));
                let tcid = self.target_attached_client(cid, prefer).ok_or("choose-tree needs an attached client")?;
                let mut items = Vec::new();
                let (cur_s, cur_w, _) = self.current(tcid);
                let mut sel = 0;
                for s in &self.sessions {
                    if Some(s.id) == cur_s && !a.has('w') {
                        sel = items.len();
                    }
                    items.push(TreeItem {
                        label: format!("{}: {} windows{}", s.name, s.windows.len(), if Some(s.id) == cur_s { " (attached)" } else { "" }),
                        session: s.id,
                        window: None,
                        depth: 0,
                    });
                    if a.has('w') || !a.has('s') {
                        for (idx, w) in &s.windows {
                            if Some(*w) == cur_w {
                                sel = items.len();
                            }
                            let agent = self
                                .windows
                                .get(w)
                                .and_then(|ww| self.panes.get(&ww.active))
                                .filter(|p| p.agent.agent.is_some())
                                .map(|p| format!("  {} {}", p.agent.state.icon(), p.agent.state.label().to_lowercase()))
                                .unwrap_or_default();
                            items.push(TreeItem { label: format!("  {}: {}{}", idx, self.window_name(*w), agent), session: s.id, window: Some(*w), depth: 1 });
                        }
                    }
                }
                if let Some(i) = self.client_idx(tcid) {
                    self.clients[i].overlay = Some(Overlay::Tree { sel, items });
                    self.clients[i].dirty = true;
                }
                Ok(Flow::Next)
            }
            "clock-mode" => {
                let t = format::strftime("%H:%M:%S");
                self.show_message(cid, &t);
                Ok(Flow::Next)
            }
            "resize-window" => {
                let a = args!("aADLRUt:x:y:");
                let (_, wid) = self.target_window(cid, a.get('t'))?;
                if let Some(w) = self.windows.get_mut(&wid) {
                    if let Some(x) = a.get('x').and_then(|v| v.parse::<u16>().ok()) {
                        w.sx = x.max(2);
                    }
                    if let Some(y) = a.get('y').and_then(|v| v.parse::<u16>().ok()) {
                        w.sy = y.max(1);
                    }
                    let (sx, sy) = (w.sx, w.sy);
                    w.layout.fit(sx, sy);
                }
                self.resize_window_panes(wid);
                Ok(Flow::Next)
            }
            "wait-for" => {
                let a = args!("LSU");
                let ch = a.pos.first().ok_or("wait-for: missing channel")?.clone();
                if a.has('S') {
                    let waiters = self.wait_channels.remove(&ch).unwrap_or_default();
                    if waiters.is_empty() {
                        self.wait_channels.insert(format!("woken:{}", ch), vec![]);
                    }
                    for w in waiters {
                        if let Some(i) = self.client_idx(w) {
                            self.clients[i].waiting = None;
                        }
                        self.run_queue(w);
                    }
                    return Ok(Flow::Next);
                }
                if self.wait_channels.remove(&format!("woken:{}", ch)).is_some() {
                    return Ok(Flow::Next);
                }
                self.wait_channels.entry(ch).or_default().push(cid);
                if let Some(i) = self.client_idx(cid) {
                    self.clients[i].waiting = Some(Wait::Events);
                }
                Ok(Flow::Wait)
            }
            "set-hook" | "show-hooks" => {
                let a = args!("agpRuwt:");
                if cmd == "set-hook"
                    && let Some(h) = a.pos.first()
                {
                    self.hooks.insert(h.clone(), a.pos[1..].to_vec());
                }
                Ok(Flow::Next)
            }
            "start-server" | "lock-server" | "server-access" | "link-window" | "unlink-window" => Ok(Flow::Next),
            "find-window" => {
                let a = args!("CiNrTZt:");
                let needle = a.pos.first().ok_or("find-window: missing match")?.to_lowercase();
                let (sid, _, _) = self.current(cid);
                let sid = sid.ok_or("no session")?;
                let wins: Vec<u32> = self.sessions.iter().find(|s| s.id == sid).map(|s| s.windows.iter().map(|(_, w)| *w).collect()).unwrap_or_default();
                for w in wins {
                    let pane_hit = self.windows.get(&w).map(|ww| ww.layout.panes()).unwrap_or_default().into_iter().find(|p| {
                        self.panes
                            .get(p)
                            .map(|pp| pp.meta.title.to_lowercase().contains(&needle) || pp.mirror.lines().iter().any(|l| l.to_lowercase().contains(&needle)))
                            .unwrap_or(false)
                    });
                    if self.window_name(w).to_lowercase().contains(&needle) || pane_hit.is_some() {
                        self.select_window(sid, w);
                        if let Some(p) = pane_hit {
                            self.select_pane(p);
                        }
                        return Ok(Flow::Next);
                    }
                }
                Err(format!("no windows matching: {}", needle))
            }
            "pipe-pane" => Err("pipe-pane is not supported yet".into()),
            // ----- amux extensions -------------------------------------
            "agent-view" => {
                let a = args!("Tf:");
                if let Some(i) = self.client_idx(cid) {
                    let c = &mut self.clients[i];
                    if c.attached.is_none() {
                        return Err("agent-view needs an attached client (try: amux agents)".into());
                    }
                    if matches!(c.overlay, Some(Overlay::Agents { .. })) && a.get('f').is_none() {
                        c.overlay = None;
                    } else {
                        c.overlay = Some(Overlay::Agents { sel: 0, tiles: a.has('T'), filter: a.get('f').unwrap_or("").to_string() });
                    }
                    c.dirty = true;
                }
                Ok(Flow::Next)
            }
            "agent-next" => {
                let rows = render::agent_rows(self, "");
                let cur = self.current(cid).2;
                let cands: Vec<u32> = rows.iter().filter(|r| matches!(r.state, AState::Waiting | AState::Done)).map(|r| r.pane).collect();
                let pick = cands.iter().find(|p| Some(**p) != cur).or(cands.first()).copied();
                match pick {
                    Some(p) => {
                        if let Some(c) = self.target_attached_client(cid, None) {
                            self.jump_to_pane(c, p);
                        }
                        Ok(Flow::Next)
                    }
                    None => {
                        self.show_message(cid, "no agents need attention");
                        Ok(Flow::Next)
                    }
                }
            }
            "agent-status" | "status" => {
                // amux status set <state> [message] | status clear   [-t pane]
                // (-t may also come last: `status set waiting "msg" -t %3`)
                let a = Args::parse_anywhere(rest, "t:").map_err(|e| format!("status: {}", e))?;
                let (_, _, pid) = self.target_pane(cid, a.get('t'))?;
                let sub = a.pos.first().map(|s| s.as_str()).unwrap_or("show");
                let owner = self.plugins.by_client(cid);
                let generic = self.agent_kinds.generic();
                // A program running in the pane that marks itself owns the mark
                // (hooks and scripts run in its process group). A job of the
                // pane's interactive shell, or anything outside the pane, does
                // not: those marks stay until cleared.
                let leader = self.panes.get(&pid).map_or(0, |p| p.child_pid);
                let mark_pg = self.client_idx(cid).and_then(|i| self.clients[i].conn.peer_pid()).and_then(|peer| {
                    let st = util::proc_stat(peer)?;
                    (leader != 0 && st.session == leader && st.pgrp != peer).then_some(st.pgrp)
                });
                let p = self.panes.get_mut(&pid).ok_or("no pane")?;
                match sub {
                    "set" => {
                        let st = a.pos.get(1).and_then(|s| AState::parse(s)).ok_or("status set: state must be working|waiting|done|idle")?;
                        let msg = a.pos[2.min(a.pos.len())..].join(" ");
                        if p.agent.agent.is_none() {
                            p.agent.agent = Some(generic);
                        }
                        let prev = p.agent.state;
                        p.agent.explicit = Some((st, msg, Instant::now()));
                        p.agent.mark_pg = mark_pg;
                        let lines = p.mirror.lines();
                        let title = p.meta.title.clone();
                        let changed = p.agent.update(Instant::now(), &lines, &title, p.exited.is_some(), util::now_unix());
                        let (now_st, summary) = (p.agent.state, p.agent.summary.clone());
                        // A plugin's statuses are its effects: undone when it unloads.
                        if let Some(o) = owner {
                            self.plugins.plugins[o].reg.statuses.insert(pid);
                        }
                        if changed {
                            let visible = self.visible_panes();
                            self.agent_changed(pid, prev, now_st, &summary, &visible);
                        }
                    }
                    "clear" => {
                        p.agent.explicit = None;
                        if let Some(o) = owner {
                            self.plugins.plugins[o].reg.statuses.remove(&pid);
                        }
                    }
                    _ => {
                        let line = format!("%{} {} {}", pid, p.agent.state.label().to_lowercase(), p.agent.summary);
                        self.output(cid, "status", vec![line]);
                        return Ok(Flow::Next);
                    }
                }
                self.redraw_all();
                Ok(Flow::Next)
            }
            "agents" => {
                let a = args!("jkw");
                if a.has('k') {
                    return self.agent_kinds_cmd(cid, a.has('j'));
                }
                if a.has('w')
                    && let Some(i) = self.client_idx(cid)
                {
                    self.clients[i].events_json = true;
                    self.clients[i].waiting = Some(Wait::Events);
                }
                let rows = render::agent_rows(self, "");
                if a.has('j') {
                    let v: Vec<serde_json::Value> = rows
                        .iter()
                        .filter_map(|r| {
                            let p = self.panes.get(&r.pane)?;
                            let s = self.sessions.iter().find(|s| s.id == r.session)?;
                            Some(serde_json::json!({
                                "pane": format!("%{}", r.pane),
                                "session": s.name,
                                "window": s.index_of(r.window),
                                "agent": p.current_command(),
                                "state": r.state.label().to_lowercase(),
                                "summary": p.agent.summary,
                                "topic": p.agent.topic,
                                "annotations": self.plugins.annotations(r.pane).into_iter().collect::<std::collections::BTreeMap<_, _>>(),
                                "cwd": p.meta.cwd,
                                "since_secs": p.agent.since.elapsed().as_secs(),
                                "command": p.meta.fg_cmd,
                            }))
                        })
                        .collect();
                    let out = serde_json::to_string_pretty(&v).unwrap_or_default();
                    self.output(cid, "agents", out.lines().map(|l| l.to_string()).collect());
                } else {
                    let mut lines = vec![format!("{:<6} {:<8} {:<10} {:<14} {:<6} SUMMARY", "PANE", "STATE", "AGENT", "WHERE", "FOR")];
                    for r in &rows {
                        let Some(p) = self.panes.get(&r.pane) else { continue };
                        let s = self
                            .sessions
                            .iter()
                            .find(|s| s.id == r.session)
                            .map(|s| format!("{}:{}", s.name, s.index_of(r.window).unwrap_or(0)))
                            .unwrap_or_default();
                        lines.push(format!(
                            "{:<6} {:<8} {:<10} {:<14} {:<6} {}",
                            format!("%{}", r.pane),
                            r.state.label().to_lowercase(),
                            p.current_command(),
                            s,
                            format!("{}s", p.agent.since.elapsed().as_secs()),
                            {
                                let tags: String = self.plugins.annotations(r.pane).iter().map(|(_, t)| format!("<{}> ", t)).collect();
                                if p.agent.topic.is_empty() || p.agent.topic == p.agent.summary {
                                    format!("{}{}", tags, p.agent.summary)
                                } else {
                                    format!("{}[{}] {}", tags, p.agent.topic, p.agent.summary)
                                }
                            }
                        ));
                    }
                    if rows.is_empty() {
                        lines = vec!["no agents running".into()];
                    }
                    self.output(cid, "agents", lines);
                }
                if a.has('w') {
                    return Ok(Flow::Wait);
                }
                Ok(Flow::Next)
            }
            "events" => {
                if let Some(i) = self.client_idx(cid) {
                    self.clients[i].events_json = true;
                    self.clients[i].waiting = Some(Wait::Events);
                }
                Ok(Flow::Wait)
            }
            "exec" => self.cmd_exec(cid, rest),
            "plugin" => self.plugin_cmd(cid, rest),
            "__migrate-import" => self.migrate_import(cid, rest),
            "await" => self.cmd_await(cid, rest),
            "panes" => self.dispatch(cid, "list-panes", &["-a".to_string()]),
            "restart-server" => {
                self.save_state();
                self.quit = Some(76);
                Ok(Flow::Next)
            }
            "panic-test" => {
                let a = args!("kt:");
                if a.has('k') {
                    let (_, _, pid) = self.target_pane(cid, a.get('t'))?;
                    if let Some(p) = self.panes.get_mut(&pid) {
                        p.send(s2k::PANIC_TEST, &[]);
                    }
                    return Ok(Flow::Next);
                }
                logf!("panic-test requested");
                panic!("panic-test: deliberate server crash");
            }
            "snapshot-panes" => {
                self.snapshot_panes(true);
                Ok(Flow::Next)
            }
            "server-info" => {
                let lines = vec![
                    format!("amux {} pid {} restarts {}", env!("CARGO_PKG_VERSION"), std::process::id(), self.restarts),
                    format!("socket {}", self.paths.server_sock().display()),
                    format!("state {}", self.paths.state_file().display()),
                    format!("sessions {} windows {} panes {} clients {}", self.sessions.len(), self.windows.len(), self.panes.len(), self.clients.len()),
                    format!("config {}", self.config_files.join(", ")),
                ];
                let lines = lines.into_iter().chain(util::prof::report()).collect();
                self.output(cid, "server", lines);
                Ok(Flow::Next)
            }
            _ => Err(format!("{}: not implemented", cmd)),
        }
    }

    fn get_buffer(&self, name: Option<&str>) -> Option<&(String, String)> {
        match name {
            Some(n) => self.buffers.iter().find(|(x, _)| x == n),
            None => self.buffers.first(),
        }
    }

    fn parse_size(&self, s: &str, total: u16) -> u16 {
        match s.strip_suffix('%') {
            Some(p) => (total as u32 * p.parse::<u32>().unwrap_or(50) / 100) as u16,
            None => s.parse().unwrap_or(total),
        }
    }

    fn fv_current(&self, cid: u32) -> Fv<'_> {
        let (s, w, p) = self.current(cid);
        Fv { srv: self, client: self.client_idx(cid).map(|i| &self.clients[i]), sid: s, wid: w, pid: p, idx: None }
    }

    /// `-c` start directory (formats expanded against `pane`).
    fn resolve_cwd(&self, cid: u32, c: Option<&str>, pane: Option<u32>, sid: Option<u32>) -> String {
        if let Some(c) = c {
            let fv = match pane {
                Some(p) => Fv::for_pane(self, None, p),
                None => self.fv_current(cid),
            };
            let v = util::expand_home(&format::expand(c, &fv));
            if !v.is_empty() {
                return v;
            }
        }
        sid.and_then(|s| self.sessions.iter().find(|x| x.id == s)).map(|s| s.cwd.clone()).filter(|s| !s.is_empty()).unwrap_or_else(util::home)
    }

    fn set_option(&mut self, cid: u32, a: &Args, window: bool) -> Result<(), String> {
        let name = a.pos.first().ok_or("set-option: missing option")?.clone();
        if !crate::options::Options::is_known(&name) {
            if a.has('q') {
                return Ok(());
            }
            return Err(format!("invalid option: {}", name));
        }
        let mut val = a.pos.get(1).cloned();
        if a.has('F')
            && let Some(v) = val.as_mut()
        {
            *v = format::expand(v, &self.fv_current(cid));
        }
        let global = a.has('g') || a.has('s') || a.get('t').is_none() && self.client_idx(cid).map(|i| self.clients[i].attached.is_none()).unwrap_or(true);
        let is_window_opt = window || a.has('w') || a.has('p');
        let map: &mut std::collections::HashMap<String, String> = if global {
            &mut self.options.global
        } else if is_window_opt {
            let wid = self.target_window(cid, a.get('t'))?.1;
            self.options.window.entry(wid).or_default()
        } else {
            let sid = self.find_session(cid, a.get('t').unwrap_or(""))?;
            self.options.session.entry(sid).or_default()
        };
        if a.has('u') || a.has('U') {
            map.remove(&name);
            if global && let Some((_, d)) = crate::options::defaults().into_iter().find(|(k, _)| *k == name) {
                map.insert(name.clone(), d);
            }
        } else if a.has('o') && map.contains_key(&name) && !global {
            return Err(format!("already set: {}", name));
        } else {
            let cur = map.get(&name).cloned().unwrap_or_default();
            let v = match val.take() {
                Some(v) => {
                    if a.has('a') {
                        let sep = if name.ends_with("style") && !cur.is_empty() { "," } else { "" };
                        format!("{}{}{}", cur, sep, v)
                    } else {
                        v
                    }
                }
                None => {
                    // flag toggle
                    if cur == "on" { "off".into() } else { "on".into() }
                }
            };
            if matches!(name.as_str(), "prefix" | "prefix2") && v != "None" && parse_key_name(&v).is_none() {
                return Err(format!("bad key: {}", v));
            }
            map.insert(name.clone(), v);
        }
        if matches!(name.as_str(), "status" | "status-position") {
            self.update_window_sizes();
        }
        if name == "plugins" {
            if self.gopt("plugins") == "off" {
                self.plugins_off();
            } else {
                self.plugins_init();
            }
        }
        for c in &mut self.clients {
            c.dirty = true;
            if matches!(name.as_str(), "status" | "status-position" | "mouse") {
                c.full_redraw = true;
            }
        }
        Ok(())
    }

    pub fn source_file(&mut self, path: &str, cid: Option<u32>) -> Vec<String> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => return vec![format!("{}: {}", path, e)],
        };
        let env: Vec<(String, String)> = self.env.clone();
        let envf = move |n: &str| env.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone()).or_else(|| std::env::var(n).ok());
        let mut cond = |c: &str| -> bool {
            let v = format::expand(c, &std::collections::HashMap::<String, String>::new());
            !v.is_empty() && v != "0"
        };
        let (lines, mut errs) = crate::cmd::parse_config(&text, &envf, &mut cond);
        errs.iter_mut().for_each(|e| *e = format!("{}: {}", path, e));
        // Config commands run on a hidden client so targets resolve sensibly.
        let tmp = cid.unwrap_or(0);
        for l in lines {
            for argv in l.cmds {
                let is_run = matches!(resolve_command(&argv[0]), Some("run-shell" | "if-shell"));
                if is_run && cid.is_none() {
                    // Never block startup on shell commands; run them in the background.
                    let cmd = argv.iter().skip(1).find(|s| !s.starts_with('-')).cloned().unwrap_or_default();
                    if resolve_command(&argv[0]) == Some("run-shell") {
                        self.start_job(&cmd, None, None, false, vec![], vec![]);
                        continue;
                    }
                }
                if let Err(e) = self.exec(tmp, argv.clone()) {
                    errs.push(format!("{}:{}: {}", path, l.line, e));
                }
            }
        }
        errs
    }

    // ----- jobs (run-shell, if-shell, copy-pipe) ------------------------

    pub fn start_job(
        &mut self,
        cmd: &str,
        client: Option<u32>,
        stdin: Option<Vec<u8>>,
        show: bool,
        on_done: Vec<Vec<String>>,
        on_fail: Vec<Vec<String>>,
    ) -> u32 {
        let id = self.next_job;
        self.next_job += 1;
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: pipe2 into a local array.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return id;
        }
        use std::os::fd::FromRawFd;
        // SAFETY: fresh descriptors.
        let (rd, wr) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let _ = util::set_nonblocking(rd.as_raw_fd(), true);
        let mut c = std::process::Command::new("/bin/sh");
        c.arg("-c").arg(cmd);
        c.env("PATH", shim_path(self));
        c.env("AMUX_SERVER", &self.paths.name);
        c.env_remove("TMUX");
        let wr2 = wr.try_clone().ok();
        c.stdout(std::process::Stdio::from(wr));
        match wr2 {
            Some(w) => c.stderr(std::process::Stdio::from(w)),
            None => c.stderr(std::process::Stdio::null()),
        };
        c.stdin(if stdin.is_some() { std::process::Stdio::piped() } else { std::process::Stdio::null() });
        match c.spawn() {
            Ok(mut ch) => {
                if let (Some(data), Some(mut si)) = (stdin, ch.stdin.take()) {
                    use std::io::Write;
                    let _ = si.write_all(&data);
                }
                drop(c);
                self.jobs.push(Job { id, pid: ch.id(), out: rd, buf: Vec::new(), client, done: None, eof: false, show, on_done, on_fail });
            }
            Err(e) => logf!("job spawn failed: {}", e),
        }
        id
    }

    pub fn read_job(&mut self, id: u32) {
        let Some(j) = self.jobs.iter_mut().find(|j| j.id == id) else { return };
        let mut b = [0u8; 8192];
        loop {
            match rustix::io::read(&j.out, &mut b) {
                Ok(0) => {
                    j.eof = true;
                    break;
                }
                Ok(n) => {
                    if j.buf.len() < 1 << 20 {
                        j.buf.extend_from_slice(&b[..n]);
                    }
                }
                Err(rustix::io::Errno::AGAIN) => break,
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => {
                    j.eof = true;
                    break;
                }
            }
        }
        if j.eof {
            if j.done.is_none() {
                // Reap promptly if the child already exited.
                let mut st = 0;
                // SAFETY: waitpid on our child.
                let r = unsafe { libc::waitpid(j.pid as i32, &mut st, libc::WNOHANG) };
                if r > 0 {
                    j.done = Some(pty::exit_code(st));
                }
            }
            if j.done.is_some() {
                self.finish_job(id);
            }
        }
    }

    pub fn finish_job(&mut self, id: u32) {
        let Some(pos) = self.jobs.iter().position(|j| j.id == id) else { return };
        let j = self.jobs.remove(pos);
        let code = j.done.unwrap_or(0);
        let text = String::from_utf8_lossy(&j.buf).into_owned();
        if let Some(cid) = j.client {
            if j.show && !text.trim().is_empty() {
                self.output(cid, "run-shell", text.lines().map(|l| l.to_string()).collect());
            }
            let cmds = if code == 0 { j.on_done } else { j.on_fail };
            if let Some(i) = self.client_idx(cid) {
                if matches!(self.clients[i].waiting, Some(Wait::Job { id: wid }) if wid == id) {
                    self.clients[i].waiting = None;
                }
                for (n, c) in cmds.into_iter().enumerate() {
                    self.clients[i].queue.insert(n, c);
                }
                self.run_queue(cid);
            }
        } else if !j.on_done.is_empty() || !j.on_fail.is_empty() {
            let cmds = if code == 0 { j.on_done } else { j.on_fail };
            for c in cmds {
                let _ = self.exec(0, c);
            }
        } else if code != 0 && !text.is_empty() {
            self.log_message(&format!("job failed ({}): {}", code, text.trim()));
        }
        self.redraw_all();
    }

    // ----- waits ------------------------------------------------------

    pub fn capture_reply(&mut self, req: u32, text: String) {
        let Some(i) = self.clients.iter().position(|c| matches!(&c.waiting, Some(Wait::Capture { req: r, .. }) if *r == req)) else { return };
        let Some(Wait::Capture { buffer, print, .. }) = self.clients[i].waiting.take() else { return };
        let cid = self.clients[i].id;
        if print {
            let c = &mut self.clients[i];
            if c.attached.is_some() {
                let lines = text.lines().map(|l| l.to_string()).collect();
                self.output(cid, "capture", lines);
            } else {
                c.out(s2c::STDOUT, text.as_bytes());
            }
        } else {
            let name = buffer.filter(|b| !b.is_empty());
            self.add_buffer(name, text);
        }
        // `amux exec`: after printing the output, remove the finished pane.
        if let Some(i) = self.client_idx(cid)
            && let Some(pane) = self.clients[i].env.iter().find(|(k, _)| k == "__AMUX_RUN_PANE").and_then(|(_, v)| v.parse::<u32>().ok())
        {
            let keep = self.clients[i].env.iter().any(|(k, _)| k == "__AMUX_RUN_KEEP");
            self.clients[i].env.retain(|(k, _)| !k.starts_with("__AMUX_RUN"));
            if !keep {
                self.destroy_pane(pane);
            }
        }
        self.run_queue(cid);
    }

    /// The pane's keeper went away (pane destroyed, or keeper replaced).
    /// Retry captures on a replacement keeper; fail waits on a gone pane.
    pub fn settle_pane_waits(&mut self, pane: u32) {
        let alive = self.panes.contains_key(&pane);
        let ids: Vec<u32> = self
            .clients
            .iter()
            .filter(|c| match &c.waiting {
                Some(Wait::Capture { pane: p, .. }) => *p == pane,
                Some(Wait::Exit { pane: p, .. } | Wait::Idle { pane: p, .. } | Wait::Pattern { pane: p, .. } | Wait::Agent { pane: p, .. }) => {
                    *p == pane && !alive
                }
                _ => false,
            })
            .map(|c| c.id)
            .collect();
        for cid in ids {
            let Some(i) = self.client_idx(cid) else { continue };
            let c = &mut self.clients[i];
            match c.waiting.take() {
                Some(Wait::Capture { argv, .. }) if alive => c.queue.push_front(argv),
                _ => {
                    c.eprint(&format!("pane %{} is gone", pane));
                    c.exit_code = 1;
                    c.queue.clear();
                }
            }
            self.run_queue(cid);
        }
    }

    pub fn check_exit_waits(&mut self, pane: u32) {
        let ids: Vec<u32> = self.clients.iter().filter(|c| matches!(&c.waiting, Some(Wait::Exit { pane: p, .. }) if *p == pane)).map(|c| c.id).collect();
        for cid in ids {
            let Some(i) = self.client_idx(cid) else { continue };
            let Some(Wait::Exit { print_output, .. }) = self.clients[i].waiting.take() else { continue };
            let code = self.panes.get(&pane).and_then(|p| p.exited).map(pty::exit_code).unwrap_or(0);
            self.clients[i].exit_code = code;
            if print_output {
                // Capture everything, then (in capture_reply) drop the pane.
                self.clients[i].env.push(("__AMUX_RUN_PANE".into(), pane.to_string()));
                self.clients[i].queue.push_front(vec![
                    "capture-pane".into(),
                    "-p".into(),
                    "-J".into(),
                    "-S".into(),
                    "-".into(),
                    "-t".into(),
                    format!("%{}", pane),
                ]);
            }
            self.run_queue(cid);
        }
    }

    pub fn check_pattern_waits(&mut self, pane: u32) {
        if !self.clients.iter().any(|c| matches!(&c.waiting, Some(Wait::Pattern { pane: p, .. }) if *p == pane)) {
            return;
        }
        let lines = match self.panes.get(&pane) {
            Some(p) => p.mirror.lines(),
            None => return,
        };
        let mut hits = Vec::new();
        for c in &self.clients {
            if let Some(Wait::Pattern { pane: p, re, .. }) = &c.waiting
                && *p == pane
                && let Some(l) = lines.iter().find(|l| re.is_match(l))
            {
                hits.push((c.id, l.clone()));
            }
        }
        for (cid, line) in hits {
            if let Some(i) = self.client_idx(cid) {
                self.clients[i].waiting = None;
                self.clients[i].print(&line);
            }
            self.run_queue(cid);
        }
    }

    /// `agents --kinds`: the agent definitions in use, in detection order.
    fn agent_kinds_cmd(&mut self, cid: u32, json: bool) -> R {
        let source = |k: &agents::Kind| match k.plugin {
            Some(id) => format!("plugin {}", self.plugins.plugins.iter().find(|p| p.id == id).map(|p| p.name.as_str()).unwrap_or("?")),
            None => "built-in".to_string(),
        };
        let lines: Vec<String> = if json {
            let v: Vec<serde_json::Value> = self
                .agent_kinds
                .all()
                .iter()
                .map(|k| {
                    let mut o = serde_json::to_value(&k.def).unwrap_or_default();
                    o["source"] = source(k).into();
                    o
                })
                .collect();
            serde_json::to_string_pretty(&v).unwrap_or_default().lines().map(String::from).collect()
        } else {
            let mut l = vec![format!("{:<14} {:<18} {:<26} MATCHES", "NAME", "SOURCE", "RESUME")];
            for k in self.agent_kinds.all() {
                l.push(format!("{:<14} {:<18} {:<26} {}", k.def.name, source(k), k.def.resume.as_deref().unwrap_or("-"), k.def.cmd));
            }
            l
        };
        self.output(cid, "agents", lines);
        Ok(Flow::Next)
    }

    pub fn check_agent_waits(&mut self) {
        let mut done = Vec::new();
        for c in &self.clients {
            if let Some(Wait::Agent { pane, states, .. }) = &c.waiting
                && let Some(p) = self.panes.get(pane)
                && states.contains(&p.agent.state)
            {
                done.push((c.id, p.agent.state.label().to_lowercase(), p.agent.summary.clone()));
            }
        }
        for (cid, st, sum) in done {
            if let Some(i) = self.client_idx(cid) {
                self.clients[i].waiting = None;
                self.clients[i].print(&format!("{} {}", st, sum));
            }
            self.run_queue(cid);
        }
    }

    pub fn check_waits(&mut self) {
        let now = Instant::now();
        let mut finished: Vec<(u32, i32)> = Vec::new();
        let mut plugin_timeouts = Vec::new();
        for c in &self.clients {
            if let Some(Wait::Plugin { plugin, req, deadline }) = &c.waiting
                && now >= *deadline
            {
                plugin_timeouts.push((c.id, *plugin, *req));
            }
            match &c.waiting {
                Some(
                    Wait::Exit { deadline: Some(d), .. }
                    | Wait::Pattern { deadline: Some(d), .. }
                    | Wait::Agent { deadline: Some(d), .. }
                    | Wait::Capture { deadline: d, .. },
                ) if now >= *d => {
                    finished.push((c.id, 124));
                }
                Some(Wait::Idle { pane, quiet, deadline }) => {
                    let last = self.panes.get(pane).and_then(|p| p.agent.last_output);
                    let idle = last.map(|t| now.duration_since(t) >= *quiet).unwrap_or(true);
                    if idle {
                        finished.push((c.id, 0));
                    } else if deadline.is_some_and(|d| now >= d) {
                        finished.push((c.id, 124));
                    }
                }
                _ => {}
            }
        }
        for (cid, code) in finished {
            if let Some(i) = self.client_idx(cid) {
                self.clients[i].waiting = None;
                if code != 0 {
                    self.clients[i].exit_code = code;
                    self.clients[i].eprint("timed out");
                }
            }
            self.run_queue(cid);
        }
        for (cid, plugin, req) in plugin_timeouts {
            self.plugin_call_timed_out(cid, plugin, req);
        }
    }

    // ----- amux CLI-level commands --------------------------------------

    /// `amux exec [-d] [-n name] [-c dir] [-t session] [-T timeout] [-k] -- cmd...`
    fn cmd_exec(&mut self, cid: u32, rest: &[String]) -> R {
        let a = Args::parse(rest, "dkn:c:t:T:").map_err(|e| format!("exec: {}", e))?;
        if a.pos.is_empty() {
            return Err("exec: missing command".into());
        }
        let sid = match a.get('t') {
            Some(t) => self.find_session(cid, t)?,
            None => match self.current(cid).0 {
                Some(s) => s,
                None => {
                    let args = Args::parse(&["-d".to_string(), "-s".into(), "runs".into()], "AdDEPXc:e:F:n:s:t:x:y:").unwrap();
                    self.create_session(cid, &args)?
                }
            },
        };
        let caller = self.client_idx(cid).and_then(|i| self.clients[i].caller_pane);
        let cwd = a.get('c').map(util::expand_home).or_else(|| self.client_idx(cid).map(|i| self.clients[i].cwd.clone())).unwrap_or_else(util::home);
        let size = self.sessions.iter().find(|s| s.id == sid).and_then(|s| self.windows.get(&s.active)).map(|w| (w.sx, w.sy)).unwrap_or((120, 40));
        let argv = if a.pos.len() == 1 { vec![a.pos[0].clone()] } else { vec![a.pos.iter().map(|s| util::shell_quote(s)).collect::<Vec<_>>().join(" ")] };
        let name = a.get('n').map(|s| s.to_string()).unwrap_or_else(|| format!("run:{}", a.pos[0].split_whitespace().next().unwrap_or("cmd")));
        let (wid, pid) = self.create_window(sid, None, Some(name), cwd, argv, size)?;
        if !a.has('d') && caller.is_none() {
            self.select_window(sid, wid);
        }
        if let Some(i) = self.client_idx(cid) {
            if a.has('k') {
                self.clients[i].env.push(("__AMUX_RUN_KEEP".into(), "1".into()));
            }
            let deadline = a.get('T').and_then(|t| t.parse::<f64>().ok()).map(|s| Instant::now() + Duration::from_secs_f64(s));
            self.clients[i].waiting = Some(Wait::Exit { pane: pid, deadline, print_output: true });
            self.clients[i].eprint(&format!("amux: running in %{} (watch with: amux attach)", pid));
        }
        Ok(Flow::Wait)
    }

    /// `amux await -t pane [-i idle-secs] [-p regex] [-x] [-s states] [-T timeout]`
    fn cmd_await(&mut self, cid: u32, rest: &[String]) -> R {
        let a = Args::parse(rest, "xi:p:s:t:T:").map_err(|e| format!("await: {}", e))?;
        let (_, _, pid) = self.target_pane(cid, a.get('t'))?;
        let deadline = a.get('T').and_then(|t| t.parse::<f64>().ok()).map(|s| Instant::now() + Duration::from_secs_f64(s));
        let w = if a.has('x') {
            if self.panes.get(&pid).map(|p| p.exited.is_some()).unwrap_or(true) {
                return Ok(Flow::Next);
            }
            Wait::Exit { pane: pid, deadline, print_output: false }
        } else if let Some(p) = a.get('p') {
            let re = regex_lite::Regex::new(p).map_err(|e| format!("bad regex: {}", e))?;
            if let Some(l) = self.panes.get(&pid).map(|pp| pp.mirror.lines()).unwrap_or_default().iter().find(|l| re.is_match(l)) {
                let l = l.clone();
                if let Some(i) = self.client_idx(cid) {
                    self.clients[i].print(&l);
                }
                return Ok(Flow::Next);
            }
            Wait::Pattern { pane: pid, re, deadline }
        } else if let Some(s) = a.get('s') {
            let states: Vec<AState> = s.split(',').filter_map(AState::parse).collect();
            if states.is_empty() {
                return Err("await -s: states are working,waiting,done,idle".into());
            }
            if self.panes.get(&pid).map(|p| states.contains(&p.agent.state)).unwrap_or(false) {
                return Ok(Flow::Next);
            }
            Wait::Agent { pane: pid, states, deadline }
        } else {
            let secs = a.get('i').and_then(|s| s.parse::<f64>().ok()).unwrap_or(2.0);
            Wait::Idle { pane: pid, quiet: Duration::from_secs_f64(secs), deadline }
        };
        if let Some(i) = self.client_idx(cid) {
            self.clients[i].waiting = Some(w);
        }
        Ok(Flow::Wait)
    }
}

fn quote_arg(s: &str) -> String {
    if s.is_empty() || s.contains(' ') || s.contains('"') || s.contains(';') || s.contains('#') {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else if s == "\\" {
        "\\\\".into()
    } else {
        s.to_string()
    }
}
