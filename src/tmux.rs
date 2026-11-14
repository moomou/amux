//! Reading a running tmux server, to migrate its sessions into amux.
//!
//! Safety: this module only *reads* tmux. [`Tmux::query`] is the one place
//! that runs tmux commands for migration, and it refuses anything outside a
//! short allowlist of commands that cannot change tmux. The live bridge
//! (`src/bridge.rs`) has its own narrow set: it attaches as a control client,
//! and only sends keys a person types.

use crate::model::{Kind, Node};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// tmux commands migration may run. `capture-pane` and `display-message` must
/// print (`-p`): without it they would store a buffer or show a message.
const READ_ONLY: &[&str] = &["list-sessions", "list-panes", "list-clients", "display-message", "capture-pane", "show-options", "show-hooks"];

/// A tmux server.
pub struct Tmux {
    pub socket: PathBuf,
}

impl Tmux {
    /// The server at `-S path` or `-L name`, else the one this runs inside
    /// ($TMUX), else tmux's default server: what a bare `tmux` would use.
    pub fn locate(socket: Option<&str>, name: Option<&str>) -> Tmux {
        if let Some(s) = socket {
            return Tmux { socket: PathBuf::from(s) };
        }
        let dir = || {
            let base = std::env::var("TMUX_TMPDIR").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/tmp".into());
            PathBuf::from(base).join(format!("tmux-{}", crate::util::uid()))
        };
        if let Some(n) = name {
            return Tmux { socket: dir().join(n) };
        }
        let sock = |var: &str| std::env::var(var).ok().and_then(|t| t.split(',').next().filter(|p| !p.is_empty()).map(PathBuf::from));
        // Inside amux with `tmux-env on`, $TMUX names amux's own server.
        if let Some(p) = sock("TMUX").filter(|p| sock("AMUX").as_ref() != Some(p)) {
            return Tmux { socket: p };
        }
        Tmux { socket: dir().join("default") }
    }

    /// Run a read-only tmux command and return its output.
    pub fn query(&self, args: &[&str]) -> Result<String, String> {
        let cmd = args.first().copied().unwrap_or("");
        if !READ_ONLY.contains(&cmd) {
            return Err(format!("refusing to run `tmux {}`: migration only reads tmux", cmd));
        }
        if matches!(cmd, "display-message" | "capture-pane") && !args.contains(&"-p") {
            return Err(format!("refusing to run `tmux {}` without -p", cmd));
        }
        self.run(args, Duration::from_secs(30))
    }

    fn run(&self, args: &[&str], timeout: Duration) -> Result<String, String> {
        let mut child = Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .args(args)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot run tmux: {}", e))?;
        let mut out = child.stdout.take().ok_or("no stdout")?;
        let mut err = child.stderr.take().ok_or("no stderr")?;
        let reader = std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = out.read_to_end(&mut b);
            b
        });
        let ereader = std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = err.read_to_end(&mut b);
            b
        });
        let start = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(st)) => break st,
                Ok(None) if start.elapsed() > timeout => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("tmux {} timed out", args.first().unwrap_or(&"")));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(e) => return Err(e.to_string()),
            }
        };
        let out = reader.join().unwrap_or_default();
        let err = ereader.join().unwrap_or_default();
        if !status.success() {
            let msg = String::from_utf8_lossy(&err).trim().to_string();
            return Err(if msg.is_empty() { format!("tmux {} failed", args.first().unwrap_or(&"")) } else { msg });
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    /// Every pane of the server, in session, window and pane order.
    pub fn panes(&self) -> Result<Vec<TPane>, String> {
        let out = self.query(&["list-panes", "-a", "-F", FIELDS])?;
        let mut v: Vec<TPane> = out.lines().filter_map(TPane::parse).collect();
        // Sessions in list-sessions order (by name), then windows and panes.
        v.sort_by(|a, b| (&a.session, a.window_index, a.pane_index).cmp(&(&b.session, b.window_index, b.pane_index)));
        Ok(v)
    }

    /// A pane's history and screen, with colours, wrapped lines joined.
    pub fn capture(&self, pane: &str, lines: usize) -> Result<String, String> {
        let start = format!("-{}", lines);
        self.query(&["capture-pane", "-p", "-e", "-J", "-t", pane, "-S", &start])
    }
}

/// Fields read for every pane (tab-separated; the title, which may contain
/// anything, comes last).
const FIELDS: &str = "#{session_id}\t#{session_name}\t#{window_id}\t#{window_index}\t#{window_name}\t#{window_active}\t\
#{window_layout}\t#{window_width}\t#{window_height}\t#{window_zoomed_flag}\t#{automatic-rename}\t#{pane_id}\t#{pane_index}\t\
#{pane_active}\t#{pane_pid}\t#{pane_current_path}\t#{pane_current_command}\t#{pane_dead}\t#{alternate_on}\t#{pane_width}\t\
#{pane_height}\t#{history_size}\t#{session_attached}\t#{pane_title}";

/// A tmux pane and its window and session.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TPane {
    pub session_id: String,
    pub session: String,
    pub window_id: String,
    pub window_index: u32,
    pub window_name: String,
    pub window_active: bool,
    pub layout: String,
    pub width: u16,
    pub height: u16,
    pub zoomed: bool,
    pub auto_name: bool,
    pub pane_id: String,
    pub pane_index: u32,
    pub active: bool,
    pub pid: u32,
    pub cwd: String,
    pub command: String,
    pub dead: bool,
    pub alternate: bool,
    pub pane_width: u16,
    pub pane_height: u16,
    pub history: u32,
    pub attached: u32,
    pub title: String,
}

impl TPane {
    fn parse(line: &str) -> Option<TPane> {
        let f: Vec<&str> = line.splitn(24, '\t').collect();
        if f.len() != 24 {
            return None;
        }
        let n = |s: &str| s.parse::<u32>().ok();
        let b = |s: &str| s == "1" || s == "on";
        Some(TPane {
            session_id: f[0].into(),
            session: f[1].into(),
            window_id: f[2].into(),
            window_index: n(f[3])?,
            window_name: f[4].into(),
            window_active: b(f[5]),
            layout: f[6].into(),
            width: n(f[7])? as u16,
            height: n(f[8])? as u16,
            zoomed: b(f[9]),
            auto_name: b(f[10]),
            pane_id: f[11].into(),
            pane_index: n(f[12])?,
            active: b(f[13]),
            pid: n(f[14])?,
            cwd: f[15].into(),
            command: f[16].trim_start_matches('-').rsplit('/').next().unwrap_or("").into(),
            dead: b(f[17]),
            alternate: b(f[18]),
            pane_width: n(f[19])? as u16,
            pane_height: n(f[20])? as u16,
            history: n(f[21]).unwrap_or(0),
            attached: n(f[22]).unwrap_or(0),
            title: f[23].into(),
        })
    }
}

// ----- layouts ----------------------------------------------------------------

/// A tmux layout (`window_layout`): sizes in cells, pane ids without `%`.
#[derive(Clone, Debug, PartialEq)]
pub enum Cell {
    Pane { w: u16, h: u16, id: u32 },
    Split { horiz: bool, w: u16, h: u16, children: Vec<Cell> },
}

impl Cell {
    fn size(&self) -> (u16, u16) {
        match self {
            Cell::Pane { w, h, .. } | Cell::Split { w, h, .. } => (*w, *h),
        }
    }

    pub fn panes(&self) -> Vec<u32> {
        match self {
            Cell::Pane { id, .. } => vec![*id],
            Cell::Split { children, .. } => children.iter().flat_map(|c| c.panes()).collect(),
        }
    }

    /// The amux layout tree, with tmux pane ids replaced by `map`.
    pub fn to_node(&self, size: u16, map: &dyn Fn(u32) -> Option<u32>) -> Result<Node, String> {
        match self {
            Cell::Pane { id, .. } => Ok(Node::pane(map(*id).ok_or(format!("pane %{} is not in this window", id))?, size)),
            Cell::Split { horiz, children, .. } => {
                let kids = children
                    .iter()
                    .map(|c| {
                        let (w, h) = c.size();
                        c.to_node(if *horiz { w } else { h }, map)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(Node { size, kind: Kind::Split { horiz: *horiz, children: kids } })
            }
        }
    }
}

/// Parse a tmux layout string such as `b25d,80x24,0,0{40x24,0,0,1,39x24,41,0,2}`.
pub fn parse_layout(s: &str) -> Result<Cell, String> {
    let body = s.split_once(',').map(|(_, b)| b).ok_or_else(|| format!("bad layout: {}", s))?;
    let mut p = LayoutParser { s: body.as_bytes(), i: 0 };
    let c = p.cell()?;
    if p.i != p.s.len() {
        return Err(format!("bad layout: {}", s));
    }
    Ok(c)
}

struct LayoutParser<'a> {
    s: &'a [u8],
    i: usize,
}

impl LayoutParser<'_> {
    fn num(&mut self) -> Result<u32, String> {
        let start = self.i;
        while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
            self.i += 1;
        }
        std::str::from_utf8(&self.s[start..self.i]).ok().and_then(|n| n.parse().ok()).ok_or_else(|| "bad layout: expected a number".to_string())
    }

    fn eat(&mut self, c: u8) -> Result<(), String> {
        if self.s.get(self.i) == Some(&c) {
            self.i += 1;
            Ok(())
        } else {
            Err(format!("bad layout: expected '{}'", c as char))
        }
    }

    fn cell(&mut self) -> Result<Cell, String> {
        let w = self.num()? as u16;
        self.eat(b'x')?;
        let h = self.num()? as u16;
        self.eat(b',')?;
        self.num()?;
        self.eat(b',')?;
        self.num()?;
        match self.s.get(self.i) {
            Some(b',') => {
                self.i += 1;
                Ok(Cell::Pane { w, h, id: self.num()? })
            }
            Some(&open @ (b'{' | b'[')) => {
                self.i += 1;
                let mut children = vec![self.cell()?];
                while self.s.get(self.i) == Some(&b',') {
                    self.i += 1;
                    children.push(self.cell()?);
                }
                self.eat(if open == b'{' { b'}' } else { b']' })?;
                Ok(Cell::Split { horiz: open == b'{', w, h, children })
            }
            _ => Err("bad layout: expected a pane id or a split".into()),
        }
    }
}

// ----- plans --------------------------------------------------------------------

/// What happens to a tmux pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Its program keeps running in tmux, shown and controlled live in amux.
    Live,
    /// A new shell in amux, in the same directory, with the pane's history.
    Fresh,
    /// The pane's program has exited (remain-on-exit): its history only.
    Dead,
}

/// A migration: what amux creates. Built by `amux migrate`, carried out by
/// the server (`__migrate-import`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Plan {
    pub socket: String,
    pub sessions: Vec<PlanSession>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PlanSession {
    pub name: String,
    pub tmux_name: String,
    pub windows: Vec<PlanWindow>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PlanWindow {
    pub index: u32,
    pub name: String,
    pub auto_name: bool,
    pub active: bool,
    pub layout: String,
    pub width: u16,
    pub height: u16,
    pub zoomed: bool,
    pub panes: Vec<PlanPane>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlanPane {
    /// The tmux pane (`%3`).
    pub id: String,
    pub mode: Mode,
    pub active: bool,
    pub cwd: String,
    /// What runs in it (`pane_current_command`).
    pub command: String,
    /// The foreground process's command line.
    pub fg_cmd: String,
    /// The pane's process and its start time (to recognise it later).
    pub pid: u32,
    pub start_ticks: u64,
    /// File with the pane's history to replay (fresh and dead panes).
    pub feed: Option<String>,
}

/// How a pane migrates by default: a shell at its prompt starts fresh,
/// anything else (an agent, an editor, a server, ssh) stays live.
pub fn classify(p: &TPane) -> Mode {
    if p.dead {
        Mode::Dead
    } else if crate::util::is_shell(&p.command) && foreground(p.pid) == Some(p.pid) && at_prompt(p.pid) {
        Mode::Fresh
    } else {
        Mode::Live
    }
}

/// A shell that runs no script or `-c` command (only options): interactive.
fn at_prompt(pid: u32) -> bool {
    crate::util::proc_cmdline(pid).is_some_and(|argv| interactive(&argv))
}

fn interactive(argv: &[String]) -> bool {
    argv.iter().skip(1).all(|a| a.starts_with('-') && a != "-c")
}

/// The foreground process group of a process's terminal (`tpgid`).
pub fn foreground(pid: u32) -> Option<u32> {
    crate::util::proc_stat(pid).and_then(|s| s.tpgid)
}

/// Whether a process is amux (this build or another).
pub fn is_amux(pid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    if crate::util::proc_comm(pid).as_deref() == Some("amux") {
        return true;
    }
    let id = |p: &Path| std::fs::metadata(p).ok().map(|m| (m.dev(), m.ino()));
    let me = std::env::current_exe().ok().and_then(|p| id(&p));
    me.is_some() && id(Path::new(&format!("/proc/{}/exe", pid))) == me
}

/// The command line of a pane's foreground process.
pub fn foreground_cmdline(pid: u32) -> String {
    let fg = foreground(pid).unwrap_or(pid);
    crate::util::proc_cmdline(fg).map(|v| v.join(" ")).unwrap_or_default()
}

/// Reasons not to bridge panes live, found before migrating: options under
/// which detaching amux's tmux client would destroy sessions, an old tmux,
/// and hooks that run when a client attaches.
pub struct Preflight {
    pub live_ok: bool,
    pub notes: Vec<String>,
    /// Sessions that must not be bridged (destroy-unattached).
    pub no_live: Vec<String>,
}

/// `sessions`: (name, id) pairs.
pub fn preflight(t: &Tmux, sessions: &[(String, String)]) -> Preflight {
    let mut pf = Preflight { live_ok: true, notes: Vec::new(), no_live: Vec::new() };
    let version = t.query(&["display-message", "-p", "#{version}"]).unwrap_or_default();
    let v = version.trim();
    let ok_version = v.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).take(2).map(|s| s.parse::<u32>().unwrap_or(0)).collect::<Vec<_>>();
    if ok_version.len() < 2 || (ok_version[0], ok_version[1]) < (3, 2) {
        pf.live_ok = false;
        pf.notes.push(format!(
            "tmux {} is older than 3.2: live panes need its control-mode flow control, so every pane starts fresh",
            if v.is_empty() { "?" } else { v }
        ));
    }
    if t.query(&["show-options", "-sv", "exit-unattached"]).map(|o| o.trim() == "on").unwrap_or(false) {
        pf.live_ok = false;
        pf.notes.push("tmux has exit-unattached on: its server exits when its last client detaches, so no pane is bridged live".into());
    }
    for (name, id) in sessions {
        // The session id: a name could match another session as a prefix.
        if t.query(&["display-message", "-p", "-t", id, "#{destroy-unattached}"]).map(|o| o.trim() == "on" || o.trim() == "1").unwrap_or(false) {
            pf.no_live.push(name.clone());
            pf.notes.push(format!("session {} has destroy-unattached on: its panes start fresh (detaching would destroy it)", name));
        }
    }
    let hooks = t.query(&["show-hooks", "-g"]).unwrap_or_default();
    for h in ["client-attached", "client-detached", "client-session-changed"] {
        // Unset hooks are listed by name only; set ones as `name[0] command`.
        if hooks.lines().any(|l| l.starts_with(&format!("{}[", h))) {
            pf.notes.push(format!("tmux runs a {} hook: it will also run when amux attaches to show live panes", h));
        }
    }
    pf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tmux_layouts() {
        assert_eq!(parse_layout("b25d,80x24,0,0,1").unwrap(), Cell::Pane { w: 80, h: 24, id: 1 });
        let l = parse_layout("6b8b,100x30,0,0{50x30,0,0,0,49x30,51,0,1}").unwrap();
        assert_eq!(l, Cell::Split { horiz: true, w: 100, h: 30, children: vec![Cell::Pane { w: 50, h: 30, id: 0 }, Cell::Pane { w: 49, h: 30, id: 1 }] });
        // Nested: a column split inside a row split.
        let l = parse_layout("1e3f,120x40,0,0{60x40,0,0,4,59x40,61,0[59x20,61,0,5,59x19,61,21,6]}").unwrap();
        assert_eq!(l.panes(), vec![4, 5, 6]);
        let node = l.to_node(120, &|id| Some(id + 100)).unwrap();
        assert_eq!(node.panes(), vec![104, 105, 106]);
        let mut rects = Vec::new();
        node.layout(crate::model::Rect { x: 0, y: 0, w: 120, h: 40 }, &mut rects);
        let r: Vec<(u32, (u16, u16, u16, u16))> = rects.iter().map(|(p, r)| (*p, (r.x, r.y, r.w, r.h))).collect();
        assert_eq!(r, vec![(104, (0, 0, 60, 40)), (105, (61, 0, 59, 20)), (106, (61, 21, 59, 19))]);
        for bad in ["", "abcd", "b25d,80x24,0,0", "b25d,80x24,0,0{40x24,0,0,1", "b25d,80x24,0,0,1junk"] {
            assert!(parse_layout(bad).is_err(), "{bad}");
        }
        assert!(l.to_node(120, &|id| (id != 5).then_some(id)).is_err());
    }

    #[test]
    fn parses_panes_and_refuses_writes() {
        let line = "$0\twork\t@1\t2\teditor\t1\tb25d,80x24,0,0,3\t80\t24\t0\ton\t%3\t0\t1\t4242\t/home/u\t-bash\t0\t0\t80\t24\t150\t1\ta\ttitle";
        let p = TPane::parse(line).unwrap();
        assert_eq!((p.session.as_str(), p.window_index, p.pane_id.as_str(), p.command.as_str(), p.title.as_str()), ("work", 2, "%3", "bash", "a\ttitle"));
        assert!(p.auto_name && p.active && !p.dead);
        assert!(TPane::parse("too\tfew").is_none());
        let t = Tmux { socket: PathBuf::from("/nonexistent/amux-test-socket") };
        for args in [&["kill-server"][..], &["send-keys", "x"], &["capture-pane", "-t", "%1"], &["display-message", "hi"], &["new-session"]] {
            assert!(t.query(args).unwrap_err().starts_with("refusing"), "{args:?}");
        }
    }

    #[test]
    fn classifies_panes() {
        let dead = TPane { dead: true, ..Default::default() };
        assert_eq!(classify(&dead), Mode::Dead);
        // Not a shell: live, whatever its process state.
        let agent = TPane { command: "claude".into(), pid: std::process::id(), ..Default::default() };
        assert_eq!(classify(&agent), Mode::Live);
        // A shell whose terminal's foreground is someone else (or unknown): live.
        let busy = TPane { command: "bash".into(), pid: u32::MAX - 1, ..Default::default() };
        assert_eq!(classify(&busy), Mode::Live);
        let argv = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(interactive(&argv(&["-bash"])) && interactive(&argv(&["bash", "--norc", "-l"])));
        assert!(!interactive(&argv(&["bash", "-c", "make"])) && !interactive(&argv(&["sh", "build.sh"])));
    }
}
