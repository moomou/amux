//! Hot-reloadable plugins.
//!
//! A plugin is a program in any language placed in the plugin directory. amux
//! runs it as a child process and talks JSON-RPC 2.0 over its stdin/stdout,
//! one JSON object per line. Plugins add commands, key bindings, event
//! handlers, status-line text, agent-dashboard annotations and agent statuses.
//! Each of these is an effect owned by the plugin and unwound when it stops,
//! reloads or crashes. Saving a plugin file reloads it (inotify, debounced).
//! A plugin can crash or hang without affecting amux: all IO is non-blocking
//! and every request has a deadline.

use super::exec::Flow;
use super::input::Binding;
use super::{Client, Overlay, Server, Wait};
use crate::agents;
use crate::cmd::{self, Args};
use crate::keys::{self, Code, Key};
use crate::paths::Paths;
use crate::proto::{Conn, s2c};
use crate::{format, logf, pty, util};
use serde_json::{Map, Value, json};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

type R = Result<Flow, String>;

pub const API: u32 = 1;
pub const GUIDE: &str = include_str!("../../docs/plugins.md");
const SDK_PY: &str = include_str!("../../sdk/python/amux_plugin.py");
const TEMPLATE_PY: &str = include_str!("../../sdk/templates/plugin.py");
const TEMPLATE_SH: &str = include_str!("../../sdk/templates/plugin.sh");

/// Events plugins can subscribe to ("*" means all of them except `output`).
pub const EVENTS: &[&str] =
    &["agent-state", "notify", "bell", "exit", "command-done", "output", "pane-new", "pane-closed", "client-attached", "client-detached"];

const DEBOUNCE: Duration = Duration::from_millis(150);
const READY_GRACE: Duration = Duration::from_millis(1500);
const TERM_AFTER: Duration = Duration::from_millis(300);
const KILL_AFTER: Duration = Duration::from_millis(1300);
/// On server shutdown: time to exit on their own, then after SIGTERM.
const SHUTDOWN_TERM: Duration = Duration::from_millis(150);
const SHUTDOWN_KILL: Duration = Duration::from_millis(400);
pub const CALL_TIMEOUT: Duration = Duration::from_secs(15);
const RUN_TIMEOUT: Duration = Duration::from_secs(30);
const LOAD_TIMEOUT: Duration = Duration::from_secs(10);
const SETTLE_TIMEOUT: Duration = Duration::from_secs(3);
const OUTPUT_INTERVAL: Duration = Duration::from_millis(250);
const CRASH_WINDOW: Duration = Duration::from_secs(300);
const MAX_CRASHES: usize = 5;
const MAX_LINE: usize = 8 << 20;
const MAX_OUTBUF: usize = 1 << 20;
const MAX_RUNS: usize = 256;
const LOG_MAX: u64 = 1 << 20;
const MAX_JUNK_LOGGED: u32 = 50;
/// Protocol messages kept per plugin for `plugin trace` and the plugin view.
const TRACE_MAX: usize = 200;
const TRACE_TEXT: usize = 400;
/// How often an open plugin view checks for changes (log output, state).
const VIEW_POLL: Duration = Duration::from_millis(300);
/// Lines the plugin view keeps for scrolling.
pub const VIEW_LINES: usize = 2000;

/// What to do once a stopping plugin's process is gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum After {
    Restart,
    Remove,
    Disable,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PState {
    /// Spawned; becomes Running when it registers or stays alive READY_GRACE.
    Starting,
    Running,
    Stopping {
        since: Instant,
        then: After,
        termed: bool,
    },
    /// Crashed; restarts at `until`.
    Backoff {
        until: Instant,
    },
    /// Inactive until its files change or `plugin reload`.
    Failed,
    /// Exited with status 0 on its own.
    Exited,
    Disabled,
}

impl PState {
    pub fn label(&self) -> &'static str {
        match self {
            PState::Starting => "starting",
            PState::Running => "running",
            PState::Stopping { then: After::Restart, .. } => "reloading",
            PState::Stopping { .. } => "stopping",
            PState::Backoff { .. } => "restarting",
            PState::Failed => "failed",
            PState::Exited => "exited",
            PState::Disabled => "disabled",
        }
    }
}

struct Proc {
    pid: u32,
    /// Start time in clock ticks since boot (/proc/PID/stat field 22).
    start_ticks: u64,
    stdin: Option<OwnedFd>,
    stdout: Option<OwnedFd>,
    inbuf: Vec<u8>,
    outbuf: Vec<u8>,
    outpos: usize,
    /// The other end of the internal client's socket (kept open).
    _peer: UnixStream,
}

/// A key binding owned by a plugin, with the binding it replaced.
pub struct KeyReg {
    table: String,
    key: Key,
    cmds: Vec<Vec<String>>,
    prev: Option<Binding>,
    pub label: String,
}

/// Everything a plugin has added to amux (unwound when it stops).
#[derive(Default)]
pub struct Registry {
    pub commands: Vec<(String, String)>,
    pub keys: Vec<KeyReg>,
    pub events: BTreeSet<String>,
    pub status: String,
    pub annotations: HashMap<u32, String>,
    /// Panes whose agent status this plugin set explicitly.
    pub statuses: HashSet<u32>,
    /// Agent definitions (by name) this plugin registered.
    pub agents: Vec<String>,
}

pub struct Plugin {
    pub id: u32,
    pub name: String,
    pub path: PathBuf,
    pub cwd: PathBuf,
    pub runner: Result<Vec<String>, String>,
    pub fingerprint: u64,
    pub state: PState,
    proc: Option<Proc>,
    /// Internal client that executes the plugin's `run` requests.
    pub client: Option<u32>,
    pub started: Option<Instant>,
    pub loads: u32,
    pub reloading: bool,
    pub reg: Registry,
    /// Commands from its last successful registration (kept after it stops).
    pub provided: Vec<String>,
    crashes: VecDeque<Instant>,
    pub restarts: u32,
    pub last_error: String,
    /// Registration problems reported to the plugin (rejected names etc.).
    pub notes: Vec<String>,
    runs: VecDeque<(Value, Vec<Vec<String>>, Option<u32>)>,
    run_inflight: Option<(Value, Instant)>,
    /// Command requests sent to the plugin: request id -> (client, command).
    calls: HashMap<u64, (u32, String)>,
    pub dropped: u64,
    junk: u32,
    /// Recent protocol messages and lifecycle events, oldest first.
    pub trace: VecDeque<TraceLine>,
    trace_seq: u64,
}

/// A protocol message or lifecycle event. `dir` is → (amux to the plugin),
/// ← (the plugin to amux), ✗ (not delivered) or · (lifecycle).
pub struct TraceLine {
    pub at: std::time::SystemTime,
    pub dir: char,
    pub text: String,
}

impl TraceLine {
    /// "HH:MM:SS.mmm dir text", for display.
    pub fn show(&self) -> String {
        let d = self.at.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        format!("{}.{:03} {} {}", format::strftime_at("%H:%M:%S", d.as_secs() as i64), d.subsec_millis(), self.dir, clean_text(&self.text))
    }
}

impl Plugin {
    /// Remember a message for the trace.
    fn note(&mut self, dir: char, text: &str) {
        if self.trace.len() >= TRACE_MAX {
            self.trace.pop_front();
        }
        let mut t: String = text.chars().take(TRACE_TEXT).collect();
        if t.len() < text.len() {
            t.push('…');
        }
        self.trace.push_back(TraceLine { at: std::time::SystemTime::now(), dir, text: t });
        self.trace_seq += 1;
    }

    pub fn pid(&self) -> Option<u32> {
        self.proc.as_ref().map(|p| p.pid)
    }

    fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.reg.commands.is_empty() {
            parts.push(format!("commands: {}", self.reg.commands.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ")));
        }
        if !self.reg.keys.is_empty() {
            parts.push(format!("keys: {}", self.reg.keys.iter().map(|k| k.label.as_str()).collect::<Vec<_>>().join(", ")));
        }
        if !self.reg.events.is_empty() {
            parts.push(format!("events: {}", self.reg.events.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
        }
        if !self.reg.agents.is_empty() {
            parts.push(format!("agents: {}", self.reg.agents.join(", ")));
        }
        parts.join(" · ")
    }
}

pub struct Host {
    pub on: bool,
    /// The server finished starting (config loaded, panes recovered).
    pub booted: bool,
    pub dir: PathBuf,
    /// Process groups of running plugins, for cleanup after a server crash.
    pid_file: PathBuf,
    pub sdk_dir: PathBuf,
    pub log_dir: PathBuf,
    pub data_dir: PathBuf,
    inotify: Option<OwnedFd>,
    watched: HashSet<PathBuf>,
    pub rescan_at: Option<Instant>,
    pub plugins: Vec<Plugin>,
    next_id: u32,
    next_req: u64,
    disabled: BTreeSet<String>,
    output_last: HashMap<u32, Instant>,
    output_pending: HashSet<u32>,
    last_housekeeping: Instant,
    /// Last time open plugin views were checked for changes.
    view_poll: Instant,
}

/// The plugin directory: $AMUX_PLUGIN_DIR, else $XDG_CONFIG_HOME/amux/plugins.
pub fn plugin_dir() -> PathBuf {
    if let Ok(d) = std::env::var("AMUX_PLUGIN_DIR")
        && !d.is_empty()
    {
        return PathBuf::from(util::expand_home(&d));
    }
    let cfg = std::env::var("XDG_CONFIG_HOME").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| format!("{}/.config", util::home()));
    PathBuf::from(cfg).join("amux").join("plugins")
}

impl Host {
    pub fn new(paths: &Paths) -> Host {
        Host {
            on: false,
            booted: false,
            dir: plugin_dir(),
            pid_file: paths.run.join("plugins.pids"),
            sdk_dir: paths.run.join("sdk"),
            log_dir: paths.state.join("plugins"),
            data_dir: paths.state.join("plugin-data"),
            inotify: None,
            watched: HashSet::new(),
            rescan_at: None,
            plugins: Vec::new(),
            next_id: 1,
            next_req: 1,
            disabled: BTreeSet::new(),
            output_last: HashMap::new(),
            output_pending: HashSet::new(),
            last_housekeeping: Instant::now(),
            view_poll: Instant::now(),
        }
    }

    fn idx(&self, id: u32) -> Option<usize> {
        self.plugins.iter().position(|p| p.id == id)
    }

    pub fn find(&self, name: &str) -> Option<usize> {
        self.plugins.iter().position(|p| p.name == name)
    }

    pub fn by_client(&self, cid: u32) -> Option<usize> {
        self.plugins.iter().position(|p| p.client == Some(cid))
    }

    /// The running plugin providing command `name`.
    pub fn command_owner(&self, name: &str) -> Option<usize> {
        self.plugins.iter().position(|p| p.state == PState::Running && p.reg.commands.iter().any(|(n, _)| n == name))
    }

    /// A reload or start is in progress (its commands may appear shortly).
    pub fn settling(&self) -> bool {
        self.rescan_at.is_some() || self.plugins.iter().any(|p| matches!(p.state, PState::Starting | PState::Stopping { then: After::Restart, .. }))
    }

    /// Done loading for `plugin reload`/`new`: up, failed, or gone.
    fn settled(&self, id: u32) -> bool {
        match self.idx(id) {
            None => true,
            Some(i) => !matches!(self.plugins[i].state, PState::Starting | PState::Stopping { .. }),
        }
    }

    pub fn wants(&self, event: &str) -> bool {
        self.on
            && self.plugins.iter().any(|p| p.state == PState::Running && (p.reg.events.contains(event) || (event != "output" && p.reg.events.contains("*"))))
    }

    /// Status-line text from all plugins.
    pub fn status_text(&self) -> String {
        let parts: Vec<&str> = self.plugins.iter().map(|p| p.reg.status.as_str()).filter(|s| !s.is_empty()).collect();
        if parts.is_empty() { String::new() } else { format!("{} ", parts.join(" ")) }
    }

    /// Annotations on a pane, in plugin order.
    pub fn annotations(&self, pane: u32) -> Vec<(&str, &str)> {
        self.plugins.iter().filter_map(|p| p.reg.annotations.get(&pane).map(|t| (p.name.as_str(), t.as_str()))).collect()
    }

    /// Commands provided by running plugins: (command, help, plugin).
    pub fn commands(&self) -> Vec<(String, String, String)> {
        let mut v = Vec::new();
        for p in self.plugins.iter().filter(|p| p.state == PState::Running) {
            for (n, h) in &p.reg.commands {
                v.push((n.clone(), h.clone(), p.name.clone()));
            }
        }
        v
    }

    pub fn deadline(&self) -> Option<Instant> {
        if !self.on {
            return None;
        }
        let mut d = self.rescan_at;
        for p in &self.plugins {
            let t = match &p.state {
                PState::Starting => p.started.map(|s| s + READY_GRACE),
                PState::Stopping { since, termed, .. } => Some(if *termed { *since + KILL_AFTER } else { *since + TERM_AFTER }),
                PState::Backoff { until } => Some(*until),
                _ => None,
            };
            d = util::earliest(d, t);
            d = util::earliest(d, p.run_inflight.as_ref().map(|(_, t)| *t + RUN_TIMEOUT));
        }
        for pane in &self.output_pending {
            d = util::earliest(d, self.output_last.get(pane).map(|t| *t + OUTPUT_INTERVAL));
        }
        d
    }

    fn watch(&mut self, dir: &Path) {
        let Some(fd) = &self.inotify else { return };
        use rustix::fs::inotify::{WatchFlags, add_watch};
        let flags = WatchFlags::CLOSE_WRITE
            | WatchFlags::MODIFY
            | WatchFlags::MOVED_TO
            | WatchFlags::MOVED_FROM
            | WatchFlags::CREATE
            | WatchFlags::DELETE
            | WatchFlags::ATTRIB
            | WatchFlags::DELETE_SELF
            | WatchFlags::MOVE_SELF;
        // Re-adding an existing watch is harmless (inotify returns the same one).
        if add_watch(fd, dir, flags).is_ok() {
            self.watched.insert(dir.to_path_buf());
        }
    }

    fn log_path(&self, name: &str) -> PathBuf {
        self.log_dir.join(format!("{}.log", name))
    }

    fn disabled_file(&self) -> PathBuf {
        self.log_dir.join("disabled")
    }

    /// Record running plugins' process groups (with start times, so a PID
    /// reused by an unrelated process is never mistaken for a plugin).
    fn save_pids(&self) {
        let text: String = self.plugins.iter().filter_map(|p| p.proc.as_ref()).map(|pr| format!("{} {}\n", pr.pid, pr.start_ticks)).collect();
        let _ = std::fs::write(&self.pid_file, text);
    }

    /// Kill what is left of plugins started by a previous server that died
    /// abruptly. Plugin processes die with the server, but their children
    /// may not, and a helper still holding a port or a lock would break the
    /// restarted plugin.
    fn reap_orphans(&self) {
        let Ok(text) = std::fs::read_to_string(&self.pid_file) else { return };
        let _ = std::fs::remove_file(&self.pid_file);
        let mut groups = Vec::new();
        for line in text.lines() {
            let mut it = line.split_whitespace();
            let (Some(pid), Some(ticks)) = (it.next().and_then(|v| v.parse::<i32>().ok()), it.next().and_then(|v| v.parse::<u64>().ok())) else {
                continue;
            };
            if pid <= 1 {
                continue;
            }
            match util::proc_stat(pid as u32) {
                Some(st) if st.start_ticks == ticks => {
                    logf!("plugins: killing process group {} left by a previous server", pid);
                    // SAFETY: a process group we started (verified by start time).
                    unsafe {
                        libc::kill(-pid, libc::SIGKILL);
                    }
                }
                // The PID belongs to another process now; the group is gone.
                Some(_) => {}
                // SAFETY: signal 0 only checks whether the group has members.
                None if unsafe { libc::kill(-pid, 0) } == 0 => groups.push(pid),
                None => {}
            }
        }
        if groups.is_empty() {
            return;
        }
        // A group id can be reused once the old group is gone, so check for
        // the environment amux gives plugins too.
        let marker = format!("AMUX_PLUGIN_LOG={}/", self.log_dir.display());
        let Ok(procs) = std::fs::read_dir("/proc") else { return };
        for e in procs.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
            if !util::proc_stat(pid).is_some_and(|st| groups.contains(&(st.pgrp as i32))) {
                continue;
            }
            let env = std::fs::read(format!("/proc/{}/environ", pid)).unwrap_or_default();
            if env.split(|&b| b == 0).any(|v| v.starts_with(marker.as_bytes())) {
                logf!("plugins: killing process {} left by a plugin of a previous server", pid);
                // SAFETY: a process started by one of our plugins (checked above).
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
            }
        }
    }

    fn save_disabled(&self) {
        let text: String = self.disabled.iter().map(|n| format!("{}\n", n)).collect();
        let _ = util::write_atomic(&self.disabled_file(), text.as_bytes());
    }
}

// ----- discovery -----------------------------------------------------------

/// A plugin found in the plugin directory.
#[derive(Debug, Clone)]
pub struct Found {
    pub name: String,
    pub path: PathBuf,
    pub cwd: PathBuf,
    pub runner: Result<Vec<String>, String>,
    pub fingerprint: u64,
    /// Directories to watch for changes (directory plugins).
    pub dirs: Vec<PathBuf>,
}

const IGNORED_EXT: &[&str] = &[
    "md", "txt", "json", "yaml", "yml", "toml", "lock", "log", "pyc", "pyo", "swp", "swo", "swx", "tmp", "bak", "orig", "rej", "disabled", "cfg", "conf",
    "ini", "csv",
];
const SKIP_DIRS: &[&str] = &["__pycache__", "node_modules", ".git", "target", "venv", ".venv"];

fn ext_of(name: &str) -> Option<&str> {
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() { None } else { Some(ext) }
}

fn stem_of(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => name,
    }
}

/// Directory entries that are never plugins (or plugin entry points).
pub fn ignored(file_name: &str) -> bool {
    let lower = file_name.to_ascii_lowercase();
    file_name.starts_with('.')
        || file_name.ends_with('~')
        || (file_name.starts_with('#') && file_name.ends_with('#'))
        || SKIP_DIRS.contains(&file_name)
        || lower.starts_with("readme")
        || lower.starts_with("license")
        || ext_of(&lower).is_some_and(|e| IGNORED_EXT.contains(&e))
}

pub fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && !s.starts_with('-') && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn interpreter(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "py" => "python3",
        "sh" => "sh",
        "bash" => "bash",
        "zsh" => "zsh",
        "js" | "mjs" | "cjs" => "node",
        "rb" => "ruby",
        "pl" => "perl",
        "lua" => "lua",
        "php" => "php",
        "tcl" => "tclsh",
        _ => return None,
    })
}

/// How to run a plugin file: directly if executable, else through its `#!`
/// line or an interpreter chosen by extension.
pub fn runner_for_file(path: &Path, meta: &std::fs::Metadata) -> Result<Vec<String>, String> {
    let p = path.to_string_lossy().into_owned();
    let exec = meta.permissions().mode() & 0o111 != 0;
    let mut head = [0u8; 256];
    let n = std::fs::File::open(path).and_then(|mut f| f.read(&mut head)).unwrap_or(0);
    let head = &head[..n];
    if head.starts_with(b"#!") {
        if exec {
            return Ok(vec![p]);
        }
        // Emulate the kernel: interpreter, then the rest of the line as one argument.
        let line = head[2..].split(|&b| b == b'\n').next().unwrap_or(&[]);
        let line = String::from_utf8_lossy(line).trim().to_string();
        let (interp, rest) = match line.split_once(char::is_whitespace) {
            Some((i, r)) => (i.to_string(), r.trim().to_string()),
            None => (line.clone(), String::new()),
        };
        if interp.is_empty() {
            return Err("empty #! line".into());
        }
        let mut v = vec![interp];
        if !rest.is_empty() {
            v.push(rest);
        }
        v.push(p);
        return Ok(v);
    }
    let fname = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
    if let Some(i) = ext_of(&fname).and_then(|e| interpreter(&e.to_ascii_lowercase())) {
        return Ok(vec![i.to_string(), p]);
    }
    if exec {
        return Ok(vec![p]);
    }
    Err("cannot tell how to run it: add a #! line, use a known extension (.py .sh .js .rb .pl .lua) or make it executable".into())
}

fn check_owner(meta: &std::fs::Metadata) -> Result<(), String> {
    if meta.uid() != util::uid() {
        return Err("refusing to run: not owned by you".into());
    }
    if meta.mode() & 0o002 != 0 {
        return Err("refusing to run: world-writable (chmod o-w)".into());
    }
    Ok(())
}

struct Fnv(u64);

impl Fnv {
    fn new() -> Fnv {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    fn add(&mut self, b: &[u8]) {
        for &x in b {
            self.0 ^= x as u64;
            self.0 = self.0.wrapping_mul(0x100_0000_01b3);
        }
    }
    fn meta(&mut self, rel: &str, m: &std::fs::Metadata) {
        self.add(rel.as_bytes());
        for v in [m.len(), m.mtime() as u64, m.mtime_nsec() as u64, m.ino(), m.mode() as u64] {
            self.add(&v.to_le_bytes());
        }
    }
}

/// Fingerprint and watch list of a directory plugin's files (bounded walk).
fn walk_dir(root: &Path, fp: &mut Fnv, dirs: &mut Vec<PathBuf>) {
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut files = 0;
    while let Some((dir, depth)) = stack.pop() {
        dirs.push(dir.clone());
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || SKIP_DIRS.contains(&name.as_str()) || name.ends_with('~') {
                continue;
            }
            let Ok(m) = std::fs::metadata(e.path()) else { continue };
            let rel = e.path().strip_prefix(root).map(|r| r.to_string_lossy().into_owned()).unwrap_or(name);
            if m.is_dir() {
                if depth < 3 && dirs.len() < 64 {
                    stack.push((e.path(), depth + 1));
                }
            } else {
                files += 1;
                if files > 2000 {
                    return;
                }
                fp.meta(&rel, &m);
            }
        }
    }
}

fn dir_entry(dir: &Path) -> Result<Vec<String>, String> {
    let run = dir.join("run");
    if let Ok(m) = std::fs::metadata(&run)
        && m.is_file()
    {
        return runner_for_file(&run, &m);
    }
    let mut names: Vec<String> =
        std::fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    names.sort();
    for want in ["main", "plugin", "index"] {
        for n in names.iter().filter(|n| stem_of(n) == want && !ignored(n)) {
            let p = dir.join(n);
            if let Ok(m) = std::fs::metadata(&p)
                && m.is_file()
                && let Ok(r) = runner_for_file(&p, &m)
            {
                return Ok(r);
            }
        }
    }
    Err("no entry point: add main.py (or main.*, plugin.*, index.*, or an executable `run`)".into())
}

/// Plugins in `dir`, sorted by file name; the first of two same-named wins.
pub fn discover(dir: &Path) -> Vec<Found> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    let mut out: Vec<Found> = Vec::new();
    for e in entries {
        let fname = e.file_name().to_string_lossy().into_owned();
        if ignored(&fname) {
            continue;
        }
        let path = e.path();
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        let mut fp = Fnv::new();
        let mut dirs = Vec::new();
        let (name, cwd, runner) = if meta.is_dir() {
            walk_dir(&path, &mut fp, &mut dirs);
            (fname.clone(), path.clone(), check_owner(&meta).and_then(|_| dir_entry(&path)))
        } else if meta.is_file() {
            fp.meta(&fname, &meta);
            (stem_of(&fname).to_string(), dir.to_path_buf(), check_owner(&meta).and_then(|_| runner_for_file(&path, &meta)))
        } else {
            continue;
        };
        let (name, runner) = if valid_name(&name) {
            (name, runner)
        } else {
            let clean: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
            (clean, Err("rename it: plugin names may only contain letters, digits, '-' and '_'".to_string()))
        };
        if out.iter().any(|f| f.name == name) {
            logf!("plugins: ignoring {} (another plugin is named {})", path.display(), name);
            continue;
        }
        fp.add(format!("{:?}", runner).as_bytes());
        out.push(Found { name, path, cwd, runner, fingerprint: fp.0, dirs });
    }
    out
}

/// Read up to `max_bytes` from the end of `path` as lines (dropping a partial
/// leading line when starting mid-file).
fn read_tail(path: &Path, max_bytes: u64) -> Vec<String> {
    let Ok(mut f) = std::fs::File::open(path) else { return Vec::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let from = len.saturating_sub(max_bytes);
    let _ = f.seek(SeekFrom::Start(from));
    let mut buf = Vec::new();
    let _ = f.take(max_bytes).read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text.lines().collect();
    if from > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    lines.into_iter().map(str::to_string).collect()
}

/// The last lines a plugin wrote to its log since it last started.
pub fn log_tail(path: &Path, n: usize) -> Vec<String> {
    let lines = read_tail(path, 16 * 1024);
    let start = lines.iter().rposition(|l| l.starts_with("--- ")).map_or(0, |i| i + 1);
    // amux's own "failed:" notes repeat what the caller reports anyway.
    let ours = |l: &str| l.starts_with("[amux ") && l.contains("] failed: ");
    let body: Vec<String> = lines[start..].iter().filter(|l| !(l.trim().is_empty() || ours(l))).cloned().collect();
    body[body.len().saturating_sub(n)..].to_vec()
}

fn append_log(path: &Path, msg: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "[amux {}] {}", format::strftime("%H:%M:%S"), msg);
    }
}

/// Keep a running plugin's log below LOG_MAX (the plugin appends to it).
fn cap_log(path: &Path) {
    let Ok(m) = std::fs::metadata(path) else { return };
    if m.len() <= LOG_MAX {
        return;
    }
    let tail = {
        let Ok(mut f) = std::fs::File::open(path) else { return };
        let _ = f.seek(SeekFrom::Start(m.len() - 64 * 1024));
        let mut b = Vec::new();
        let _ = f.read_to_end(&mut b);
        b
    };
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).truncate(true).open(path) {
        let _ = f.write_all(b"[amux] log truncated (over 1 MB)\n");
        let _ = f.write_all(&tail);
    }
}

fn json_str(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or("").to_string()
}

/// Text a plugin returned for a command, as lines.
fn result_lines(v: Option<&Value>) -> Vec<String> {
    match v {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(s)) => s.lines().map(str::to_string).collect(),
        Some(Value::Array(a)) => a.iter().map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string())).collect(),
        Some(Value::Object(o)) => match o.get("output").or_else(|| o.get("text")).and_then(Value::as_str) {
            Some(s) => s.lines().map(str::to_string).collect(),
            None => serde_json::to_string_pretty(o).unwrap_or_default().lines().map(str::to_string).collect(),
        },
        Some(other) => vec![other.to_string()],
    }
}

/// A `key=value` argument of `plugin emit` as JSON (numbers and booleans typed).
fn emit_value(v: &str) -> Value {
    match v {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ => v.parse::<i64>().map(Value::from).unwrap_or_else(|_| Value::String(v.to_string())),
    }
}

fn pane_ref(s: &str) -> Option<u32> {
    s.strip_prefix('%').unwrap_or(s).parse().ok()
}

/// Whether a child process has exited, without reaping it.
fn exited(pid: u32) -> bool {
    // SAFETY: waitid with WNOWAIT on a zeroed siginfo; the child stays waitable.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) != 0 || info.si_pid() != 0
    }
}

fn sanitize(s: &str, max: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(max).collect()
}

fn flush_proc(pr: &mut Proc) {
    let Some(fd) = pr.stdin.as_ref() else {
        pr.outbuf.clear();
        pr.outpos = 0;
        return;
    };
    while pr.outpos < pr.outbuf.len() {
        match rustix::io::write(fd, &pr.outbuf[pr.outpos..]) {
            Ok(n) => pr.outpos += n,
            Err(rustix::io::Errno::AGAIN) => break,
            Err(rustix::io::Errno::INTR) => continue,
            Err(_) => {
                // The plugin closed stdin (or died): drop what it will never read.
                pr.outbuf.clear();
                pr.outpos = 0;
                pr.stdin = None;
                return;
            }
        }
    }
    if pr.outpos == pr.outbuf.len() {
        pr.outbuf.clear();
        pr.outpos = 0;
    } else if pr.outpos > 64 * 1024 {
        pr.outbuf.drain(..pr.outpos);
        pr.outpos = 0;
    }
}

// ----- lifecycle -------------------------------------------------------------

impl Server {
    /// Start the plugin system (after the config is loaded).
    pub fn plugins_init(&mut self) {
        if !self.plugins.booted || self.gopt("plugins") == "off" || self.plugins.on {
            return;
        }
        let h = &mut self.plugins;
        if let Err(e) = std::fs::create_dir_all(&h.dir) {
            logf!("plugins: cannot create {}: {}", h.dir.display(), e);
            return;
        }
        for d in [&h.log_dir, &h.data_dir, &h.sdk_dir] {
            let _ = std::fs::create_dir_all(d);
        }
        let sdk = h.sdk_dir.join("amux_plugin.py");
        if std::fs::read(&sdk).ok().as_deref() != Some(SDK_PY.as_bytes()) {
            let _ = util::write_atomic(&sdk, SDK_PY.as_bytes());
        }
        h.disabled = std::fs::read_to_string(h.disabled_file()).unwrap_or_default().lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
        use rustix::fs::inotify::{CreateFlags, init};
        h.inotify = init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK).ok();
        if h.inotify.is_none() {
            logf!("plugins: inotify unavailable; use `amux plugin reload` after edits");
        }
        h.on = true;
        h.reap_orphans();
        logf!("plugins: watching {}", h.dir.display());
        self.plugins_rescan();
    }

    /// Stop every plugin and the watcher (`set -g plugins off`).
    pub fn plugins_off(&mut self) {
        let ids: Vec<u32> = self.plugins.plugins.iter().map(|p| p.id).collect();
        for id in ids {
            self.plugin_stop(id, After::Remove, "plugins turned off");
        }
        self.plugins.inotify = None;
        self.plugins.watched.clear();
        self.plugins.rescan_at = None;
        self.plugins.on = false;
    }

    /// Read pending inotify events and apply changes now (commands that
    /// depend on the current set of plugins call this first).
    pub fn plugins_sync(&mut self) {
        if !self.plugins.on {
            return;
        }
        self.plugins_drain_inotify();
        if self.plugins.rescan_at.is_some() {
            self.plugins_rescan();
        }
    }

    fn plugins_drain_inotify(&mut self) {
        let Some(fd) = &self.plugins.inotify else { return };
        let mut buf = [0u8; 4096];
        let mut any = false;
        while let Ok(n) = rustix::io::read(fd, &mut buf) {
            if n == 0 {
                break;
            }
            any = true;
        }
        if any {
            // Debounce: editors and agents write in several steps.
            self.plugins.rescan_at = Some(Instant::now() + DEBOUNCE);
        }
    }

    /// Compare the plugin directory with the running plugins: start new ones,
    /// reload changed ones, stop removed ones.
    pub fn plugins_rescan(&mut self) {
        self.plugins.rescan_at = None;
        if !self.plugins.on {
            return;
        }
        let dir = self.plugins.dir.clone();
        self.plugins.watched.retain(|p| p.exists());
        self.plugins.watch(&dir);
        let found = discover(&dir);
        for f in &found {
            for d in &f.dirs {
                self.plugins.watch(d);
            }
        }
        let gone: Vec<u32> = self.plugins.plugins.iter().filter(|p| !found.iter().any(|f| f.name == p.name)).map(|p| p.id).collect();
        for id in gone {
            self.plugin_stop(id, After::Remove, "its file was removed");
        }
        for f in found {
            let Some(i) = self.plugins.find(&f.name) else {
                let id = self.plugins.next_id;
                self.plugins.next_id += 1;
                let disabled = self.plugins.disabled.contains(&f.name);
                self.plugins.plugins.push(Plugin {
                    id,
                    name: f.name,
                    path: f.path,
                    cwd: f.cwd,
                    runner: f.runner,
                    fingerprint: f.fingerprint,
                    state: if disabled { PState::Disabled } else { PState::Exited },
                    proc: None,
                    client: None,
                    started: None,
                    loads: 0,
                    reloading: false,
                    reg: Registry::default(),
                    provided: Vec::new(),
                    crashes: VecDeque::new(),
                    restarts: 0,
                    last_error: String::new(),
                    notes: Vec::new(),
                    runs: VecDeque::new(),
                    run_inflight: None,
                    calls: HashMap::new(),
                    dropped: 0,
                    junk: 0,
                    trace: VecDeque::new(),
                    trace_seq: 0,
                });
                if !disabled {
                    self.plugin_start(id);
                }
                continue;
            };
            let p = &mut self.plugins.plugins[i];
            let changed = p.fingerprint != f.fingerprint || p.path != f.path;
            p.path = f.path;
            p.cwd = f.cwd;
            p.runner = f.runner;
            p.fingerprint = f.fingerprint;
            if changed {
                let id = p.id;
                self.plugin_reload(id);
            }
        }
    }

    /// Restart a plugin with its current files (a failed one gets a new chance).
    fn plugin_reload(&mut self, id: u32) {
        let Some(i) = self.plugins.idx(id) else { return };
        let p = &mut self.plugins.plugins[i];
        match p.state.clone() {
            PState::Disabled => {}
            PState::Stopping { since, termed, .. } => p.state = PState::Stopping { since, termed, then: After::Restart },
            PState::Running | PState::Starting => {
                p.reloading = true;
                p.crashes.clear();
                self.plugin_stop(id, After::Restart, "reloading");
            }
            PState::Backoff { .. } | PState::Failed | PState::Exited => {
                p.reloading = p.loads > 0;
                p.crashes.clear();
                self.plugin_start(id);
            }
        }
    }

    fn plugin_start(&mut self, id: u32) {
        let Some(i) = self.plugins.idx(id) else { return };
        let (name, runner, cwd) = {
            let p = &self.plugins.plugins[i];
            (p.name.clone(), p.runner.clone(), p.cwd.clone())
        };
        let argv = match runner {
            Ok(a) if !a.is_empty() => a,
            Ok(_) => return self.plugin_failed(id, "nothing to run".into()),
            Err(e) => return self.plugin_failed(id, e),
        };
        let log_path = self.plugins.log_path(&name);
        if std::fs::metadata(&log_path).map(|m| m.len() > LOG_MAX).unwrap_or(false) {
            let _ = std::fs::rename(&log_path, log_path.with_extension("log.1"));
        }
        let data = self.plugins.data_dir.join(&name);
        let _ = std::fs::create_dir_all(&data);
        let mut log = match std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
            Ok(f) => f,
            Err(e) => return self.plugin_failed(id, format!("cannot open log {}: {}", log_path.display(), e)),
        };
        let _ = writeln!(log, "--- {} starting: {}", format::strftime("%Y-%m-%d %H:%M:%S"), argv.join(" "));
        let (ours, peer) = match UnixStream::pair() {
            Ok(p) => p,
            Err(e) => return self.plugin_failed(id, format!("socketpair: {}", e)),
        };
        let sdk = self.plugins.sdk_dir.to_string_lossy().into_owned();
        let pypath = match std::env::var("PYTHONPATH") {
            Ok(p) if !p.is_empty() => format!("{}:{}", sdk, p),
            _ => sdk.clone(),
        };
        let mut c = std::process::Command::new(&argv[0]);
        c.args(&argv[1..])
            .current_dir(&cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::from(log))
            .env("PATH", format!("{}:{}", self.paths.shim_dir().display(), std::env::var("PATH").unwrap_or_default()))
            .env("PYTHONPATH", pypath)
            .env("PYTHONUNBUFFERED", "1")
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env("AMUX_SERVER", &self.paths.name)
            .env("AMUX_PLUGIN", &name)
            .env("AMUX_PLUGIN_API", API.to_string())
            .env("AMUX_PLUGIN_DIR", &self.plugins.dir)
            .env("AMUX_PLUGIN_DATA", &data)
            .env("AMUX_PLUGIN_LOG", &log_path)
            .env("AMUX_PLUGIN_SDK", &sdk);
        for k in ["TMUX", "TMUX_PANE", "AMUX", "AMUX_PANE"] {
            c.env_remove(k);
        }
        use std::os::unix::process::CommandExt;
        // Own process group (stopped as a whole). It dies with the server:
        // a restarted server must not find a second copy still running.
        c.process_group(0);
        let server = std::process::id();
        // SAFETY: prctl, getppid and _exit are async-signal-safe.
        unsafe {
            c.pre_exec(move || {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                // The server died before the death signal was armed.
                if libc::getppid() as u32 != server {
                    libc::_exit(1);
                }
                Ok(())
            });
        }
        let mut child = match c.spawn() {
            Ok(ch) => ch,
            Err(e) => return self.plugin_failed(id, format!("cannot run `{}`: {}", argv[0], e)),
        };
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return self.plugin_failed(id, "no pipes".into());
        };
        let (stdin, stdout): (OwnedFd, OwnedFd) = (stdin.into(), stdout.into());
        let _ = util::set_nonblocking(stdin.as_raw_fd(), true);
        let _ = util::set_nonblocking(stdout.as_raw_fd(), true);
        let pid = child.id();
        // The server reaps children itself (SIGCHLD); dropping Child does not wait.
        drop(child);
        let cid = self.next_client;
        self.next_client += 1;
        match Conn::new(ours) {
            Ok(conn) => {
                let mut cl = Client::new(cid, conn);
                cl.cli = true;
                cl.plugin = Some(id);
                cl.sink = Some(Vec::new());
                cl.cwd = util::home();
                self.clients.push(cl);
            }
            Err(e) => logf!("plugin {}: internal client: {}", name, e),
        }
        let p = &mut self.plugins.plugins[i];
        let start_ticks = util::proc_stat(pid).map_or(0, |st| st.start_ticks);
        p.proc = Some(Proc { pid, start_ticks, stdin: Some(stdin), stdout: Some(stdout), inbuf: Vec::new(), outbuf: Vec::new(), outpos: 0, _peer: peer });
        p.client = Some(cid);
        p.state = PState::Starting;
        p.started = Some(Instant::now());
        p.reg = Registry::default();
        p.notes.clear();
        p.junk = 0;
        self.plugins.save_pids();
        logf!("plugin {} started (pid {}): {}", name, pid, argv.join(" "));
        if let Some(i) = self.plugins.idx(id) {
            self.plugins.plugins[i].note('·', &format!("started (pid {}): {}", pid, argv.join(" ")));
        }
        let hello = json!({
            "api": API,
            "name": name,
            "amux": env!("CARGO_PKG_VERSION"),
            "server": self.paths.name,
            "plugin_dir": self.plugins.dir,
            "data_dir": data,
        });
        self.plugin_notify(id, "hello", hello);
    }

    /// Stop a plugin: unwind its effects now, end its process, then `then`.
    fn plugin_stop(&mut self, id: u32, then: After, why: &str) {
        let Some(i) = self.plugins.idx(id) else { return };
        if let PState::Stopping { since, termed, .. } = self.plugins.plugins[i].state {
            self.plugins.plugins[i].state = PState::Stopping { since, termed, then };
            return;
        }
        let name = self.plugins.plugins[i].name.clone();
        self.plugins.plugins[i].note('·', &format!("stopping: {}", why));
        self.plugin_unwind(id, &format!("plugin {} stopped ({})", name, why));
        let Some(i) = self.plugins.idx(id) else { return };
        if self.plugins.plugins[i].proc.is_some() {
            self.plugin_notify(id, "shutdown", json!({}));
            let p = &mut self.plugins.plugins[i];
            if let Some(pr) = p.proc.as_mut() {
                flush_proc(pr);
                pr.stdin = None;
            }
            p.state = PState::Stopping { since: Instant::now(), then, termed: false };
            logf!("plugin {} stopping: {}", name, why);
        } else {
            self.plugin_after(id, then);
        }
    }

    /// The process is gone: carry out what the stop was for.
    fn plugin_after(&mut self, id: u32, then: After) {
        let Some(i) = self.plugins.idx(id) else { return };
        match then {
            After::Restart => self.plugin_start(id),
            After::Remove => {
                let name = self.plugins.plugins.remove(i).name;
                logf!("plugin {} removed", name);
                self.status_message_all(&format!("plugin {} removed", name));
            }
            After::Disable => self.plugins.plugins[i].state = PState::Disabled,
        }
    }

    /// Undo everything the plugin registered and fail its pending requests.
    fn plugin_unwind(&mut self, id: u32, why: &str) {
        let Some(i) = self.plugins.idx(id) else { return };
        let p = &mut self.plugins.plugins[i];
        let reg = std::mem::take(&mut p.reg);
        let calls: Vec<(u64, (u32, String))> = p.calls.drain().collect();
        p.runs.clear();
        p.run_inflight = None;
        let client = p.client.take();
        for k in reg.keys.iter().rev() {
            self.restore_key(k);
        }
        if !reg.status.is_empty() || !reg.annotations.is_empty() {
            self.status_gen += 1;
        }
        for pane in &reg.statuses {
            if let Some(pn) = self.panes.get_mut(pane) {
                pn.agent.explicit = None;
            }
        }
        if !reg.agents.is_empty() {
            self.agent_kinds.set_plugin(id, Vec::new());
            self.agents_redetect();
        }
        if let Some(cid) = client {
            self.clients.retain(|c| c.id != cid);
        }
        for (req, (cid, _)) in calls {
            self.plugin_call_failed(cid, req, why);
        }
        self.redraw_all();
    }

    fn restore_key(&mut self, k: &KeyReg) {
        let ours = self.keys.tables.get(&k.table).and_then(|t| t.get(&k.key)).is_some_and(|b| b.cmds == k.cmds);
        if !ours {
            return; // rebound since: leave the newer binding alone
        }
        match &k.prev {
            Some(b) => {
                self.keys.tables.entry(k.table.clone()).or_default().insert(k.key, b.clone());
            }
            None => self.keys.unbind(&k.table, k.key),
        }
    }

    fn plugin_failed(&mut self, id: u32, error: String) {
        let Some(i) = self.plugins.idx(id) else { return };
        let p = &mut self.plugins.plugins[i];
        p.note('·', &format!("failed: {}", error));
        p.state = PState::Failed;
        p.last_error = error.clone();
        p.reloading = false;
        let name = p.name.clone();
        logf!("plugin {} failed: {}", name, error);
        append_log(&self.plugins.log_path(&name), &format!("failed: {}", error));
        self.status_message_all(&format!("plugin {} failed: {}", name, error));
    }

    /// The plugin registered (or stayed up): it is live.
    fn plugin_ready(&mut self, id: u32) {
        let Some(i) = self.plugins.idx(id) else { return };
        let p = &mut self.plugins.plugins[i];
        if p.state != PState::Starting {
            return;
        }
        p.state = PState::Running;
        p.loads += 1;
        p.last_error.clear();
        let verb = if p.reloading { "reloaded" } else { "loaded" };
        p.reloading = false;
        let summary = p.summary();
        let name = p.name.clone();
        logf!("plugin {} {} {}", name, verb, summary);
        let msg = if summary.is_empty() { format!("plugin {} {}", name, verb) } else { format!("plugin {} {} · {}", name, verb, summary) };
        self.status_message_all(&msg);
    }

    /// SIGCHLD for a plugin process.
    pub fn plugins_reaped(&mut self, pid: u32, status: i32) {
        let Some(i) = self.plugins.plugins.iter().position(|p| p.proc.as_ref().is_some_and(|pr| pr.pid == pid)) else { return };
        let id = self.plugins.plugins[i].id;
        // Messages written just before exiting still count.
        self.plugin_read(id);
        let Some(i) = self.plugins.idx(id) else { return };
        self.plugins.plugins[i].proc = None;
        self.plugins.save_pids();
        // SAFETY: signalling the plugin's own process group (leftover children).
        unsafe {
            libc::kill(-(pid as i32), libc::SIGTERM);
        }
        let code = pty::exit_code(status);
        self.plugins.plugins[i].note('·', &format!("exited (status {})", code));
        let state = self.plugins.plugins[i].state.clone();
        if let PState::Stopping { then, .. } = state {
            self.plugin_after(id, then);
            return;
        }
        let name = self.plugins.plugins[i].name.clone();
        let tail = log_tail(&self.plugins.log_path(&name), 8);
        let reason = tail.last().map(|l| sanitize(l.trim(), 200)).filter(|l| !l.is_empty()).map(|l| format!(": {}", l)).unwrap_or_default();
        self.plugin_unwind(id, &format!("plugin {} exited (status {}){}", name, code, reason));
        let Some(i) = self.plugins.idx(id) else { return };
        if state == PState::Starting {
            return self.plugin_failed(id, format!("exited during startup (status {}){}", code, reason));
        }
        let p = &mut self.plugins.plugins[i];
        if code == 0 {
            p.state = PState::Exited;
            p.last_error = "exited with status 0".into();
            logf!("plugin {} exited", name);
            return;
        }
        let now = Instant::now();
        p.crashes.push_back(now);
        while p.crashes.front().is_some_and(|t| now.duration_since(*t) > CRASH_WINDOW) {
            p.crashes.pop_front();
        }
        if p.crashes.len() >= MAX_CRASHES {
            return self.plugin_failed(id, format!("crashed {} times in 5 minutes (status {}){}", MAX_CRASHES, code, reason));
        }
        let delay = Duration::from_secs(1 << (p.crashes.len() - 1).min(5));
        p.state = PState::Backoff { until: now + delay };
        p.restarts += 1;
        p.reloading = true;
        p.last_error = format!("crashed (status {}){}", code, reason);
        logf!("plugin {} crashed (status {}); restarting in {:?}", name, code, delay);
        self.status_message_all(&format!("plugin {} crashed (status {}){}; restarting in {}s", name, code, reason, delay.as_secs()));
    }

    pub fn plugins_timers(&mut self) {
        if !self.plugins.on {
            return;
        }
        let now = Instant::now();
        if self.plugins.rescan_at.is_some_and(|t| now >= t) {
            self.plugins_rescan();
        }
        let (mut ready, mut start, mut stuck) = (Vec::new(), Vec::new(), Vec::new());
        for p in &mut self.plugins.plugins {
            match &mut p.state {
                PState::Starting if p.started.is_some_and(|t| now >= t + READY_GRACE) && p.proc.is_some() => ready.push(p.id),
                PState::Stopping { since, termed, .. } => {
                    if let Some(pr) = &p.proc {
                        let sig = if now >= *since + KILL_AFTER {
                            libc::SIGKILL
                        } else if !*termed && now >= *since + TERM_AFTER {
                            *termed = true;
                            libc::SIGTERM
                        } else {
                            0
                        };
                        if sig != 0 {
                            // SAFETY: signalling the plugin's own process group.
                            unsafe {
                                libc::kill(-(pr.pid as i32), sig);
                            }
                        }
                    }
                }
                PState::Backoff { until } if now >= *until => start.push(p.id),
                _ => {}
            }
            if p.run_inflight.as_ref().is_some_and(|(_, t)| now >= *t + RUN_TIMEOUT) {
                stuck.push(p.id);
            }
        }
        for id in ready {
            self.plugin_ready(id);
        }
        for id in start {
            self.plugin_start(id);
        }
        for id in stuck {
            self.plugin_run_timed_out(id);
        }
        self.plugins_flush_output(now);
        if now.duration_since(self.plugins.last_housekeeping) >= Duration::from_secs(10) {
            self.plugins.last_housekeeping = now;
            for p in self.plugins.plugins.iter().filter(|p| p.proc.is_some()) {
                cap_log(&self.plugins.log_path(&p.name));
            }
        }
        self.plugins_check_waits();
    }

    /// Stop all plugins (server shutdown). They get a moment to exit on
    /// their own (running atexit handlers, saving state), then SIGTERM, then
    /// SIGKILL; the delay only applies to plugins that ignore `shutdown`.
    pub fn plugins_shutdown(&mut self) {
        let ids: Vec<u32> = self.plugins.plugins.iter().filter(|p| p.proc.is_some()).map(|p| p.id).collect();
        let mut pids = Vec::new();
        for id in ids {
            self.plugin_notify(id, "shutdown", json!({}));
            if let Some(i) = self.plugins.idx(id)
                && let Some(pr) = self.plugins.plugins[i].proc.as_mut()
            {
                flush_proc(pr);
                pr.stdin = None;
                pids.push(pr.pid);
            }
        }
        let start = Instant::now();
        let mut live = pids.clone();
        let mut termed = false;
        while !live.is_empty() && start.elapsed() < SHUTDOWN_KILL {
            if !termed && start.elapsed() >= SHUTDOWN_TERM {
                termed = true;
                for &pid in &live {
                    // SAFETY: the plugin's own process group (leader not reaped).
                    unsafe {
                        libc::kill(-(pid as i32), libc::SIGTERM);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(5));
            live.retain(|&pid| !exited(pid));
        }
        // Also ends what plugins left behind. Leaders are still unreaped, so
        // their group ids cannot have been reused.
        for pid in pids {
            // SAFETY: as above; then reap the leader if it is gone.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
                libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG);
            }
        }
        let _ = std::fs::remove_file(&self.plugins.pid_file);
    }

    // ----- IO --------------------------------------------------------------

    /// Descriptors to poll: (plugin id, kind, fd, events); kind 0 = watcher,
    /// 1 = plugin stdout, 2 = plugin stdin (when output is pending).
    pub fn plugins_poll_fds(&self, out: &mut Vec<(u32, u8, RawFd, i16)>) {
        if !self.plugins.on {
            return;
        }
        if let Some(fd) = &self.plugins.inotify {
            out.push((0, 0, fd.as_raw_fd(), util::POLLIN));
        }
        for p in &self.plugins.plugins {
            let Some(pr) = &p.proc else { continue };
            if let Some(fd) = &pr.stdout {
                out.push((p.id, 1, fd.as_raw_fd(), util::POLLIN));
            }
            if let Some(fd) = &pr.stdin
                && pr.outpos < pr.outbuf.len()
            {
                out.push((p.id, 2, fd.as_raw_fd(), util::POLLOUT));
            }
        }
    }

    pub fn plugins_on_ready(&mut self, id: u32, kind: u8) {
        match kind {
            0 => self.plugins_drain_inotify(),
            1 => self.plugin_read(id),
            2 => {
                if let Some(i) = self.plugins.idx(id)
                    && let Some(pr) = self.plugins.plugins[i].proc.as_mut()
                {
                    flush_proc(pr);
                }
            }
            _ => {}
        }
    }

    fn plugin_read(&mut self, id: u32) {
        let mut lines: Vec<Vec<u8>> = Vec::new();
        let mut too_long = false;
        {
            let Some(i) = self.plugins.idx(id) else { return };
            let Some(pr) = self.plugins.plugins[i].proc.as_mut() else { return };
            let mut eof = false;
            if let Some(fd) = pr.stdout.as_ref() {
                let mut total = 0;
                loop {
                    pr.inbuf.reserve(16 * 1024);
                    match rustix::io::read(fd, rustix::buffer::spare_capacity(&mut pr.inbuf)) {
                        Ok(0) => {
                            eof = true;
                            break;
                        }
                        Ok(n) => {
                            total += n;
                            if total >= 1 << 20 {
                                break;
                            }
                        }
                        Err(rustix::io::Errno::AGAIN) => break,
                        Err(rustix::io::Errno::INTR) => continue,
                        Err(_) => {
                            eof = true;
                            break;
                        }
                    }
                }
            }
            if eof {
                pr.stdout = None;
            }
            let mut start = 0;
            while let Some(off) = memchr::memchr(b'\n', &pr.inbuf[start..]) {
                lines.push(pr.inbuf[start..start + off].to_vec());
                start += off + 1;
            }
            pr.inbuf.drain(..start);
            if pr.inbuf.len() > MAX_LINE {
                pr.inbuf.clear();
                too_long = true;
            }
        }
        for l in lines {
            let live = self.plugins.idx(id).is_some_and(|i| matches!(self.plugins.plugins[i].state, PState::Starting | PState::Running));
            if !live {
                break;
            }
            self.plugin_line(id, &l);
        }
        if too_long {
            let name = self.plugins.idx(id).map(|i| self.plugins.plugins[i].name.clone()).unwrap_or_default();
            append_log(&self.plugins.log_path(&name), "protocol error: a line longer than 8 MB; stopping");
            self.plugin_stop(id, After::Disable, "protocol error");
            if let Some(i) = self.plugins.idx(id) {
                self.plugins.plugins[i].last_error = "protocol error: line longer than 8 MB".into();
            }
        }
    }

    /// Queue a line for the plugin; false if it is gone or not reading.
    fn plugin_send_line(&mut self, id: u32, line: &str) -> bool {
        let Some(i) = self.plugins.idx(id) else { return false };
        let p = &mut self.plugins.plugins[i];
        let Some(pr) = p.proc.as_mut() else { return false };
        if pr.stdin.is_none() || pr.outbuf.len() - pr.outpos > MAX_OUTBUF {
            p.dropped += 1;
            p.note('✗', line);
            return false;
        }
        pr.outbuf.extend_from_slice(line.as_bytes());
        pr.outbuf.push(b'\n');
        flush_proc(pr);
        p.note('→', line);
        true
    }

    fn plugin_notify(&mut self, id: u32, method: &str, params: Value) -> bool {
        self.plugin_send_line(id, &format!("{{\"jsonrpc\":\"2.0\",\"method\":{},\"params\":{}}}", Value::from(method), params))
    }

    fn plugin_respond(&mut self, id: u32, rid: Option<Value>, res: Result<Value, String>) {
        let Some(rid) = rid else { return };
        let line = match res {
            Ok(v) => format!("{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{}}}", rid, v),
            Err(e) => format!("{{\"jsonrpc\":\"2.0\",\"id\":{},\"error\":{}}}", rid, json!({"code": 1, "message": e})),
        };
        self.plugin_send_line(id, &line);
    }

    // ----- protocol --------------------------------------------------------

    fn plugin_line(&mut self, id: u32, line: &[u8]) {
        let line = line.trim_ascii();
        if line.is_empty() {
            return;
        }
        if let Some(i) = self.plugins.idx(id) {
            self.plugins.plugins[i].note('←', &String::from_utf8_lossy(line));
        }
        match serde_json::from_slice::<Value>(line) {
            Ok(msg @ Value::Object(_)) => self.plugin_message(id, msg),
            _ => {
                // Stray stdout output (a print() in a raw-protocol plugin):
                // keep it visible in the log instead of failing the plugin.
                let Some(i) = self.plugins.idx(id) else { return };
                let log = self.plugins.log_path(&self.plugins.plugins[i].name);
                let p = &mut self.plugins.plugins[i];
                p.junk += 1;
                if p.junk <= MAX_JUNK_LOGGED {
                    let text = sanitize(&String::from_utf8_lossy(line), 300);
                    let note = if p.junk == MAX_JUNK_LOGGED { " (further stray output is not logged)" } else { "" };
                    append_log(&log, &format!("stdout is for protocol messages; ignored: {}{}", text, note));
                }
            }
        }
    }

    fn plugin_message(&mut self, id: u32, msg: Value) {
        let rid = msg.get("id").filter(|v| !v.is_null()).cloned();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let Some(method) = msg.get("method").and_then(Value::as_str).map(str::to_string) else {
            if let Some(req) = rid.as_ref().and_then(Value::as_u64) {
                self.plugin_reply(id, req, &msg);
            }
            return;
        };
        let res: Result<Value, String> = match method.as_str() {
            "register" => Ok(self.plugin_register(id, &params)),
            "run" => return self.plugin_queue_run(id, rid, &params),
            "status" => {
                let text = sanitize(&json_str(params.get("text")), 200);
                if let Some(i) = self.plugins.idx(id)
                    && self.plugins.plugins[i].reg.status != text
                {
                    self.plugins.plugins[i].reg.status = text;
                    self.status_gen += 1;
                    self.redraw_all();
                }
                Ok(Value::Bool(true))
            }
            "annotate" => match params.get("pane").and_then(Value::as_str).and_then(pane_ref) {
                Some(pane) if self.panes.contains_key(&pane) => {
                    let text = sanitize(&json_str(params.get("text")), 60);
                    if let Some(i) = self.plugins.idx(id) {
                        let a = &mut self.plugins.plugins[i].reg.annotations;
                        if text.is_empty() {
                            a.remove(&pane);
                        } else {
                            a.insert(pane, text);
                        }
                    }
                    self.redraw_all();
                    Ok(Value::Bool(true))
                }
                _ => Err("annotate: params need an existing \"pane\" like \"%3\"".into()),
            },
            "notify" => {
                let msg = sanitize(&json_str(params.get("message")), 300);
                let name = self.plugins.idx(id).map(|i| self.plugins.plugins[i].name.clone()).unwrap_or_default();
                if !msg.is_empty() {
                    self.notify_clients(&msg);
                    self.status_message_all(&format!("{}: {}", name, msg));
                }
                Ok(Value::Bool(true))
            }
            "log" => {
                if let Some(i) = self.plugins.idx(id) {
                    append_log(&self.plugins.log_path(&self.plugins.plugins[i].name), &json_str(params.get("message")));
                }
                Ok(Value::Bool(true))
            }
            other => Err(format!("unknown method: {} (see `amux plugin guide`)", other)),
        };
        self.plugin_respond(id, rid, res);
    }

    /// `register`: replace the plugin's commands, key bindings and events.
    fn plugin_register(&mut self, id: u32, params: &Value) -> Value {
        let Some(i) = self.plugins.idx(id) else { return Value::Null };
        let old = std::mem::take(&mut self.plugins.plugins[i].reg.keys);
        for k in old.iter().rev() {
            self.restore_key(k);
        }
        let name = self.plugins.plugins[i].name.clone();
        let mut rejected: Vec<Value> = Vec::new();
        let none = Vec::new();
        let mut commands: Vec<(String, String)> = Vec::new();
        for c in params.get("commands").and_then(Value::as_array).unwrap_or(&none) {
            let (cname, help) = match c {
                Value::String(s) => (s.clone(), String::new()),
                Value::Object(o) => (json_str(o.get("name")), sanitize(&json_str(o.get("help")), 200)),
                _ => continue,
            };
            match self.plugin_command_name_ok(id, &cname) {
                Err(why) => rejected.push(json!({"name": cname, "reason": why})),
                Ok(()) if commands.iter().any(|(n, _)| *n == cname) => {}
                Ok(()) => commands.push((cname, help)),
            }
        }
        let mut events = BTreeSet::new();
        for e in params.get("events").and_then(Value::as_array).unwrap_or(&none) {
            let e = e.as_str().unwrap_or("");
            if e == "*" || EVENTS.contains(&e) {
                events.insert(e.to_string());
            } else {
                rejected.push(json!({"name": e, "reason": format!("unknown event; known: {}", EVENTS.join(", "))}));
            }
        }
        let mut keys_reg = Vec::new();
        for k in params.get("keys").and_then(Value::as_array).unwrap_or(&none) {
            let kname = json_str(k.get("key"));
            let table = Some(json_str(k.get("table"))).filter(|t| !t.is_empty()).unwrap_or_else(|| "prefix".into());
            let Some(key) = keys::parse_key_name(&kname) else {
                rejected.push(json!({"name": kname, "reason": "unknown key name (examples: g, M-x, C-g, F5)"}));
                continue;
            };
            let cmds = match cmd::parse_cmdlist(&json_str(k.get("command")), &|_| None) {
                Ok(c) if !c.is_empty() => c,
                _ => {
                    rejected.push(json!({"name": kname, "reason": "key needs a \"command\""}));
                    continue;
                }
            };
            let note = Some(json_str(k.get("note"))).filter(|n| !n.is_empty()).unwrap_or_else(|| format!("plugin {}", name));
            let prev = self.keys.tables.get(&table).and_then(|t| t.get(&key)).cloned();
            self.keys.bind(&table, key, cmds.clone(), false, &note);
            keys_reg.push(KeyReg { label: format!("{} {}", table, keys::key_name(key)), table, key, cmds, prev });
        }
        let mut kinds: Vec<agents::Kind> = Vec::new();
        for a in params.get("agents").and_then(Value::as_array).unwrap_or(&none) {
            let label = format!("agent {}", json_str(a.get("name")));
            let def = match serde_json::from_value::<agents::Def>(a.clone()) {
                Ok(d) => d,
                Err(e) => {
                    rejected.push(json!({"name": label, "reason": e.to_string()}));
                    continue;
                }
            };
            if kinds.iter().any(|k| k.def.name == def.name) {
                continue;
            }
            if let Some(other) = self.agent_kinds.owner(&def.name).filter(|o| *o != id) {
                let who = self.plugins.idx(other).map(|j| self.plugins.plugins[j].name.clone()).unwrap_or_default();
                rejected.push(json!({"name": label, "reason": format!("already defined by plugin {}", who)}));
                continue;
            }
            match agents::Kind::compile(def, Some(id)) {
                Ok(k) => kinds.push(k),
                Err(why) => rejected.push(json!({"name": label, "reason": why})),
            }
        }
        let agent_names: Vec<String> = kinds.iter().map(|k| k.def.name.clone()).collect();
        let Some(i) = self.plugins.idx(id) else { return Value::Null };
        let log = self.plugins.log_path(&name);
        let p = &mut self.plugins.plugins[i];
        p.notes = rejected.iter().map(|r| format!("rejected {}: {}", json_str(r.get("name")), json_str(r.get("reason")))).collect();
        for n in &p.notes {
            append_log(&log, n);
        }
        let result = json!({
            "commands": commands.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
            "keys": keys_reg.iter().map(|k| k.label.clone()).collect::<Vec<_>>(),
            "events": events.iter().cloned().collect::<Vec<_>>(),
            "agents": agent_names,
            "rejected": rejected,
        });
        let first = p.state == PState::Starting;
        p.provided = commands.iter().map(|(n, _)| n.clone()).collect();
        p.reg.commands = commands;
        p.reg.events = events;
        p.reg.keys = keys_reg;
        let had_agents = !std::mem::replace(&mut p.reg.agents, agent_names).is_empty();
        if had_agents || !kinds.is_empty() {
            self.agent_kinds.set_plugin(id, kinds);
            self.agents_redetect();
        }
        if first {
            self.plugin_ready(id);
        } else {
            logf!("plugin {} registered: {}", name, self.plugins.plugins[i].summary());
        }
        result
    }

    fn plugin_command_name_ok(&self, id: u32, name: &str) -> Result<(), String> {
        if !valid_name(name) {
            return Err("invalid name: use letters, digits, '-' and '_'".into());
        }
        if cmd::is_reserved_command(name) {
            return Err("reserved: it is (or abbreviates) a built-in command".into());
        }
        if let Some(o) = self.plugins.plugins.iter().find(|p| p.id != id && p.state == PState::Running && p.reg.commands.iter().any(|(n, _)| n == name)) {
            return Err(format!("already provided by plugin {}", o.name));
        }
        Ok(())
    }

    // ----- run: plugins executing amux commands ------------------------------

    fn plugin_queue_run(&mut self, id: u32, rid: Option<Value>, params: &Value) {
        let cmds: Result<Vec<Vec<String>>, String> = if let Some(argv) = params.get("argv").and_then(Value::as_array) {
            let v: Vec<String> = argv.iter().map(|a| a.as_str().map(str::to_string).unwrap_or_else(|| a.to_string())).collect();
            Ok(if v.is_empty() { Vec::new() } else { vec![v] })
        } else if let Some(s) = params.get("command").and_then(Value::as_str) {
            cmd::parse_cmdlist(s, &|_| None)
        } else {
            Err("run: params need \"argv\" (a list) or \"command\" (a string)".into())
        };
        let cmds: Vec<Vec<String>> = match cmds {
            Ok(c) if !c.is_empty() => c.into_iter().map(crate::cli::translate_long_options).collect(),
            Ok(_) => return self.plugin_respond(id, rid, Err("run: empty command".into())),
            Err(e) => return self.plugin_respond(id, rid, Err(e)),
        };
        let pane = params.get("pane").and_then(Value::as_str).and_then(pane_ref);
        let Some(i) = self.plugins.idx(id) else { return };
        if self.plugins.plugins[i].runs.len() >= MAX_RUNS {
            return self.plugin_respond(id, rid, Err("run: too many pending requests".into()));
        }
        self.plugins.plugins[i].runs.push_back((rid.unwrap_or(Value::Null), cmds, pane));
        self.plugins_pump();
    }

    /// Start queued `run` requests (one at a time per plugin).
    pub fn plugins_pump(&mut self) {
        let mut budget = 10_000;
        loop {
            let ids: Vec<u32> =
                self.plugins.plugins.iter().filter(|p| p.run_inflight.is_none() && !p.runs.is_empty() && p.client.is_some()).map(|p| p.id).collect();
            if ids.is_empty() || budget == 0 {
                break;
            }
            for id in ids {
                budget -= 1;
                self.plugin_start_run(id);
                if budget == 0 {
                    break;
                }
            }
        }
    }

    fn plugin_start_run(&mut self, id: u32) {
        let Some(i) = self.plugins.idx(id) else { return };
        let p = &mut self.plugins.plugins[i];
        let (Some(cid), Some((rid, cmds, pane))) = (p.client, p.runs.pop_front()) else { return };
        p.run_inflight = Some((rid, Instant::now()));
        let cwd = pane.and_then(|pn| self.panes.get(&pn)).map(|pn| pn.meta.cwd.clone()).filter(|c| !c.is_empty()).unwrap_or_else(util::home);
        let Some(ci) = self.client_idx(cid) else { return };
        let c = &mut self.clients[ci];
        c.caller_pane = pane.filter(|p| self.panes.contains_key(p));
        c.cwd = cwd;
        c.exit_code = 0;
        c.waiting = None;
        c.prompt = None;
        c.sink = Some(Vec::new());
        c.queue = cmds.into();
        self.run_queue(cid);
    }

    /// The internal client finished a `run` request: reply with its output.
    pub fn plugin_run_finished(&mut self, cid: u32) {
        let Some(i) = self.plugins.by_client(cid) else { return };
        let id = self.plugins.plugins[i].id;
        let Some((rid, _)) = self.plugins.plugins[i].run_inflight.take() else { return };
        let Some(ci) = self.client_idx(cid) else { return };
        let c = &mut self.clients[ci];
        let frames = c.sink.replace(Vec::new()).unwrap_or_default();
        let code = std::mem::replace(&mut c.exit_code, 0);
        c.caller_pane = None;
        let (mut out, mut err) = (Vec::new(), Vec::new());
        for (ty, b) in frames {
            match ty {
                s2c::STDOUT => out.extend_from_slice(&b),
                s2c::STDERR => err.extend_from_slice(&b),
                _ => {}
            }
        }
        if !rid.is_null() {
            let res = json!({"stdout": String::from_utf8_lossy(&out), "stderr": String::from_utf8_lossy(&err), "code": code});
            self.plugin_respond(id, Some(rid), Ok(res));
        }
    }

    fn plugin_run_timed_out(&mut self, id: u32) {
        let Some(i) = self.plugins.idx(id) else { return };
        let Some((rid, _)) = self.plugins.plugins[i].run_inflight.take() else { return };
        if let Some(cid) = self.plugins.plugins[i].client
            && let Some(ci) = self.client_idx(cid)
        {
            let c = &mut self.clients[ci];
            c.waiting = None;
            c.prompt = None;
            c.overlay = None;
            c.queue.clear();
            c.sink = Some(Vec::new());
            c.exit_code = 0;
        }
        if !rid.is_null() {
            self.plugin_respond(id, Some(rid), Err(format!("run: no result after {}s", RUN_TIMEOUT.as_secs())));
        }
        self.plugins_pump();
    }

    // ----- commands provided by plugins ---------------------------------------

    /// A command no built-in knows: a plugin's command, or wait briefly if a
    /// plugin is (re)loading (it may be about to provide it).
    pub fn exec_unknown(&mut self, cid: u32, argv: Vec<String>) -> R {
        let name = argv[0].clone();
        if self.plugins.on {
            self.plugins_sync();
            if let Some(i) = self.plugins.command_owner(&name) {
                return self.plugin_call(cid, i, &name, &argv[1..]);
            }
            if self.plugins.settling() && self.plugin_settle_wait(cid, &argv) {
                return Ok(Flow::Wait);
            }
            if let Some(p) = self.plugins.plugins.iter().find(|p| p.provided.contains(&name)) {
                let why = if p.last_error.is_empty() { String::new() } else { format!(": {}", p.last_error) };
                return Err(format!("{}: plugin {} is {}{} (amux plugin show {})", name, p.name, p.state.label(), why, p.name));
            }
        }
        Err(format!("unknown command: {}", name))
    }

    fn plugin_call(&mut self, cid: u32, i: usize, name: &str, args: &[String]) -> R {
        let (sid, wid, pid) = self.current(cid);
        let mut params = Map::new();
        params.insert("name".into(), name.into());
        params.insert("args".into(), args.into());
        if let Some(p) = pid {
            params.insert("pane".into(), format!("%{}", p).into());
        }
        if let Some(s) = sid.and_then(|s| self.sessions.iter().find(|x| x.id == s)) {
            params.insert("session".into(), s.name.clone().into());
            if let Some(w) = wid.and_then(|w| s.index_of(w)) {
                params.insert("window".into(), w.into());
            }
        }
        let client = self.client_idx(cid).map(|ci| &self.clients[ci]);
        let cwd = pid.and_then(|p| self.panes.get(&p)).map(|p| p.meta.cwd.clone()).filter(|c| !c.is_empty()).or_else(|| client.map(|c| c.cwd.clone()));
        params.insert("cwd".into(), cwd.unwrap_or_default().into());
        params.insert("client".into(), format!("client{}", cid).into());
        params.insert("attached".into(), client.is_some_and(|c| c.attached.is_some()).into());
        let (id, pname) = (self.plugins.plugins[i].id, self.plugins.plugins[i].name.clone());
        let req = self.plugins.next_req;
        self.plugins.next_req += 1;
        let line = format!("{{\"jsonrpc\":\"2.0\",\"id\":{},\"method\":\"command\",\"params\":{}}}", req, Value::Object(params));
        if !self.plugin_send_line(id, &line) {
            return Err(format!("plugin {} is not reading its input", pname));
        }
        if let Some(i) = self.plugins.idx(id) {
            self.plugins.plugins[i].calls.insert(req, (cid, name.to_string()));
        }
        if let Some(ci) = self.client_idx(cid) {
            self.clients[ci].waiting = Some(Wait::Plugin { plugin: id, req, deadline: Instant::now() + CALL_TIMEOUT });
        }
        Ok(Flow::Wait)
    }

    /// A plugin answered one of our `command` requests.
    fn plugin_reply(&mut self, id: u32, req: u64, msg: &Value) {
        let Some(i) = self.plugins.idx(id) else { return };
        let Some((cid, cname)) = self.plugins.plugins[i].calls.remove(&req) else { return };
        let Some(ci) = self.client_idx(cid) else { return };
        if !matches!(self.clients[ci].waiting, Some(Wait::Plugin { req: r, .. }) if r == req) {
            return;
        }
        self.clients[ci].waiting = None;
        match msg.get("error").filter(|e| !e.is_null()) {
            Some(err) => {
                let text = err.get("message").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| err.to_string());
                self.command_failed(cid, &format!("{}: {}", cname, text));
            }
            None => self.output(cid, &cname, result_lines(msg.get("result"))),
        }
        self.run_queue(cid);
    }

    fn plugin_call_failed(&mut self, cid: u32, req: u64, why: &str) {
        let Some(ci) = self.client_idx(cid) else { return };
        if !matches!(self.clients[ci].waiting, Some(Wait::Plugin { req: r, .. }) if r == req) {
            return;
        }
        self.clients[ci].waiting = None;
        self.command_failed(cid, why);
        self.run_queue(cid);
    }

    /// Report a failed command like built-in commands do.
    pub fn command_failed(&mut self, cid: u32, text: &str) {
        let Some(ci) = self.client_idx(cid) else { return };
        if self.clients[ci].attached.is_some() {
            self.show_message(cid, text);
        } else {
            self.clients[ci].eprint(text);
        }
        if let Some(ci) = self.client_idx(cid) {
            let c = &mut self.clients[ci];
            c.exit_code = 1;
            c.queue.clear();
        }
    }

    /// A `command` request timed out (called from check_waits).
    pub fn plugin_call_timed_out(&mut self, cid: u32, plugin: u32, req: u64) {
        let name = match self.plugins.idx(plugin) {
            Some(i) => {
                self.plugins.plugins[i].calls.remove(&req);
                self.plugins.plugins[i].name.clone()
            }
            None => "?".into(),
        };
        self.plugin_call_failed(cid, req, &format!("plugin {} did not answer within {}s", name, CALL_TIMEOUT.as_secs()));
    }

    // ----- events ----------------------------------------------------------------

    /// Send an event to subscribed plugins; returns how many got it.
    pub fn plugins_event(&mut self, event: &str, pane: Option<u32>, mut params: Map<String, Value>) -> usize {
        if !self.plugins.wants(event) {
            return 0;
        }
        if let Some(p) = pane {
            for (k, v) in self.pane_info(p) {
                params.entry(k).or_insert(v);
            }
        }
        params.insert("event".into(), event.into());
        params.insert("time".into(), util::now_ms().into());
        let line = format!("{{\"jsonrpc\":\"2.0\",\"method\":\"event\",\"params\":{}}}", Value::Object(params));
        let ids: Vec<u32> = self
            .plugins
            .plugins
            .iter()
            .filter(|p| p.state == PState::Running && (p.reg.events.contains(event) || (event != "output" && p.reg.events.contains("*"))))
            .map(|p| p.id)
            .collect();
        ids.into_iter().filter(|id| self.plugin_send_line(*id, &line)).count()
    }

    /// What plugins get to know about a pane.
    pub fn pane_info(&self, pane: u32) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("pane".into(), format!("%{}", pane).into());
        let Some(p) = self.panes.get(&pane) else { return m };
        if let Some(w) = self.window_of_pane(pane)
            && let Some(s) = self.session_of_window(w).and_then(|sid| self.sessions.iter().find(|s| s.id == sid))
        {
            m.insert("session".into(), s.name.clone().into());
            if let Some(idx) = s.index_of(w) {
                m.insert("window".into(), idx.into());
            }
        }
        m.insert("cwd".into(), p.meta.cwd.clone().into());
        m.insert("command".into(), p.current_command().into());
        m.insert("title".into(), p.mirror.title.clone().into());
        if let Some(k) = &p.agent.agent {
            m.insert("agent".into(), k.def.name.clone().into());
            m.insert("state".into(), p.agent.state.label().to_lowercase().into());
            m.insert("summary".into(), p.agent.summary.clone().into());
            m.insert("topic".into(), p.agent.topic.clone().into());
        }
        m
    }

    /// A pane is gone: drop plugin state that refers to it.
    pub fn plugins_forget_pane(&mut self, pane: u32) {
        let mut changed = false;
        for p in &mut self.plugins.plugins {
            changed |= p.reg.annotations.remove(&pane).is_some();
            p.reg.statuses.remove(&pane);
        }
        self.plugins.output_last.remove(&pane);
        self.plugins.output_pending.remove(&pane);
        if changed {
            self.status_gen += 1;
        }
    }

    /// A pane's screen changed: `output` events, at most every 250 ms per pane.
    pub fn plugins_note_output(&mut self, pane: u32) {
        if !self.plugins.wants("output") || self.plugins.output_pending.contains(&pane) {
            return;
        }
        let now = Instant::now();
        if self.plugins.output_last.get(&pane).is_some_and(|t| now.duration_since(*t) < OUTPUT_INTERVAL) {
            self.plugins.output_pending.insert(pane);
        } else {
            self.plugins_send_output(pane, now);
        }
    }

    fn plugins_send_output(&mut self, pane: u32, now: Instant) {
        self.plugins.output_last.insert(pane, now);
        let Some(p) = self.panes.get(&pane) else { return };
        let mut lines: Vec<String> = (0..p.mirror.rows).rev().map(|y| p.mirror.row_text(y)).filter(|l| !l.trim().is_empty()).take(5).collect();
        lines.reverse();
        let mut params = Map::new();
        params.insert("lines".into(), lines.into());
        self.plugins_event("output", Some(pane), params);
    }

    fn plugins_flush_output(&mut self, now: Instant) {
        if self.plugins.output_pending.is_empty() {
            return;
        }
        let due: Vec<u32> = self
            .plugins
            .output_pending
            .iter()
            .copied()
            .filter(|p| self.plugins.output_last.get(p).is_none_or(|t| now.duration_since(*t) >= OUTPUT_INTERVAL))
            .collect();
        for pane in due {
            self.plugins.output_pending.remove(&pane);
            if self.panes.contains_key(&pane) {
                self.plugins_send_output(pane, now);
            }
        }
        let panes = &self.panes;
        self.plugins.output_last.retain(|p, _| panes.contains_key(p));
    }

    // ----- waiting for plugins ----------------------------------------------------

    /// Hold `argv` until in-progress (re)loads finish, once. True if waiting.
    fn plugin_settle_wait(&mut self, cid: u32, argv: &[String]) -> bool {
        let Some(ci) = self.client_idx(cid) else { return false };
        let c = &mut self.clients[ci];
        if c.settle_retry {
            return false;
        }
        c.waiting = Some(Wait::PluginSettle { argv: argv.to_vec(), deadline: Instant::now() + SETTLE_TIMEOUT });
        true
    }

    pub fn plugins_check_waits(&mut self) {
        let now = Instant::now();
        let settling = self.plugins.settling();
        let mut retry = Vec::new();
        let mut loaded = Vec::new();
        for c in &self.clients {
            match &c.waiting {
                Some(Wait::PluginSettle { argv, deadline }) if !settling || now >= *deadline => retry.push((c.id, argv.clone())),
                Some(Wait::PluginLoad { ids, deadline }) if now >= *deadline || ids.iter().all(|id| self.plugins.settled(*id)) => {
                    loaded.push((c.id, ids.clone()))
                }
                _ => {}
            }
        }
        for (cid, argv) in retry {
            if let Some(ci) = self.client_idx(cid) {
                let c = &mut self.clients[ci];
                c.waiting = None;
                c.settle_retry = true;
                c.queue.push_front(argv);
            }
            self.run_queue(cid);
        }
        for (cid, ids) in loaded {
            let mut lines = Vec::new();
            let mut failed = false;
            for id in ids {
                let Some(i) = self.plugins.idx(id) else { continue };
                let (l, ok) = self.plugin_report(i);
                failed |= !ok;
                lines.extend(l);
            }
            if let Some(ci) = self.client_idx(cid) {
                self.clients[ci].waiting = None;
                if failed {
                    self.clients[ci].exit_code = 1;
                }
            }
            self.output(cid, "plugin", lines);
            self.run_queue(cid);
        }
    }

    /// One plugin's outcome after a load: lines, and whether it is up.
    fn plugin_report(&self, i: usize) -> (Vec<String>, bool) {
        let p = &self.plugins.plugins[i];
        let mut lines = Vec::new();
        let ok = match &p.state {
            PState::Running => {
                let s = p.summary();
                lines.push(if s.is_empty() { format!("{}: running", p.name) } else { format!("{}: running · {}", p.name, s) });
                lines.extend(p.notes.iter().map(|n| format!("  {}", n)));
                p.notes.is_empty()
            }
            PState::Disabled => {
                lines.push(format!("{}: disabled (amux plugin enable {})", p.name, p.name));
                true
            }
            PState::Exited => {
                lines.push(format!("{}: exited with status 0", p.name));
                true
            }
            PState::Starting | PState::Stopping { .. } => {
                lines.push(format!("{}: still {} after {}s (see: amux plugin logs {})", p.name, p.state.label(), LOAD_TIMEOUT.as_secs(), p.name));
                false
            }
            PState::Failed | PState::Backoff { .. } => {
                lines.push(format!("{}: {}: {}", p.name, p.state.label(), p.last_error));
                let tail = log_tail(&self.plugins.log_path(&p.name), 12);
                if !tail.is_empty() {
                    lines.push(format!("  log ({}):", self.plugins.log_path(&p.name).display()));
                    lines.extend(tail.into_iter().map(|l| format!("    {}", l)));
                }
                false
            }
        };
        (lines, ok)
    }

    // ----- `amux plugin ...` ---------------------------------------------------------

    pub fn plugin_cmd(&mut self, cid: u32, rest: &[String]) -> R {
        const USAGE: &str = "usage: plugin [list | show NAME | view [NAME] | new NAME [-l python|sh] | reload [NAME...] | logs NAME [-n LINES] | \
             trace NAME [-n LINES] | enable NAME | disable NAME | install PATH [-f] | remove NAME | emit EVENT [-t PANE] [KEY=VALUE...] | dir | guide]";
        let sub = rest.first().map(|s| s.as_str()).unwrap_or("list");
        let args: Vec<String> = rest.iter().skip(1).cloned().collect();
        let argv: Vec<String> = std::iter::once("plugin".to_string()).chain(std::iter::once(sub.to_string())).chain(args.iter().cloned()).collect();
        match sub {
            "guide" | "help" => {
                self.output(cid, "amux plugin guide", GUIDE.lines().map(str::to_string).collect());
                return Ok(Flow::Next);
            }
            "dir" => {
                self.output(cid, "plugin dir", vec![self.plugins.dir.display().to_string()]);
                return Ok(Flow::Next);
            }
            _ => {}
        }
        if !self.plugins.on {
            return Err("plugins are off (set -g plugins on)".into());
        }
        self.plugins_sync();
        let one = |args: &[String]| -> Result<String, String> { args.first().cloned().ok_or_else(|| USAGE.to_string()) };
        match sub {
            "list" | "ls" => {
                if self.plugins.settling() && self.plugin_settle_wait(cid, &argv) {
                    return Ok(Flow::Wait);
                }
                let lines = self.plugin_table();
                self.output(cid, "plugins", lines);
                Ok(Flow::Next)
            }
            "show" | "info" => {
                let name = one(&args)?;
                if self.plugins.settling() && self.plugin_settle_wait(cid, &argv) {
                    return Ok(Flow::Wait);
                }
                let i = self.plugin_named(&name)?;
                let lines = self.plugin_details(i);
                self.output(cid, &format!("plugin {}", name), lines);
                Ok(Flow::Next)
            }
            "view" | "ui" => {
                if let Some(n) = args.first() {
                    self.plugin_named(n)?;
                }
                self.plugin_view_open(cid, args.first().cloned())
            }
            "trace" => {
                let a = Args::parse_anywhere(&args, "n:").map_err(|e| format!("plugin trace: {}", e))?;
                let name = one(&a.pos)?;
                let i = self.plugin_named(&name)?;
                let n: usize = a.get('n').and_then(|v| v.parse().ok()).unwrap_or(40);
                let t = &self.plugins.plugins[i].trace;
                let lines: Vec<String> = t.iter().skip(t.len().saturating_sub(n)).map(TraceLine::show).collect();
                self.output(cid, &format!("plugin {} trace", name), if lines.is_empty() { vec!["(no messages yet)".into()] } else { lines });
                Ok(Flow::Next)
            }
            "new" | "create" => self.plugin_new(cid, &args),
            "reload" | "restart" => {
                let ids: Vec<u32> = if args.is_empty() {
                    self.plugins.plugins.iter().filter(|p| p.state != PState::Disabled).map(|p| p.id).collect()
                } else {
                    let mut v = Vec::new();
                    for n in &args {
                        let i = self.plugin_named(n)?;
                        v.push(self.plugins.plugins[i].id);
                    }
                    v
                };
                if ids.is_empty() {
                    self.output(cid, "plugins", vec![format!("no plugins in {}", self.plugins.dir.display())]);
                    return Ok(Flow::Next);
                }
                for id in &ids {
                    self.plugin_reload(*id);
                }
                self.plugin_wait_load(cid, ids)
            }
            "logs" | "log" => {
                let a = Args::parse_anywhere(&args, "n:").map_err(|e| format!("plugin logs: {}", e))?;
                let name = one(&a.pos)?;
                self.plugin_named(&name)?;
                let n: usize = a.get('n').and_then(|v| v.parse().ok()).unwrap_or(40);
                let all = read_tail(&self.plugins.log_path(&name), (n as u64 * 512).clamp(16 * 1024, 256 * 1024));
                let lines = all[all.len().saturating_sub(n)..].to_vec();
                self.output(cid, &format!("plugin {} log", name), if lines.is_empty() { vec!["(log is empty)".into()] } else { lines });
                Ok(Flow::Next)
            }
            "disable" => {
                let name = one(&args)?;
                let i = self.plugin_named(&name)?;
                self.plugin_disable(self.plugins.plugins[i].id);
                self.output(cid, "plugin", vec![format!("{} disabled (persistent; amux plugin enable {})", name, name)]);
                Ok(Flow::Next)
            }
            "enable" => {
                let name = one(&args)?;
                let i = self.plugin_named(&name)?;
                let id = self.plugins.plugins[i].id;
                self.plugin_enable(id);
                self.plugin_wait_load(cid, vec![id])
            }
            "install" => self.plugin_install(cid, &args),
            "remove" | "rm" | "uninstall" => {
                let name = one(&args)?;
                let i = self.plugin_named(&name)?;
                let path = self.plugins.plugins[i].path.clone();
                let dest = self.plugin_trash(&path)?;
                self.plugins_rescan();
                self.output(cid, "plugin", vec![format!("removed {} (moved to {})", name, dest.display())]);
                Ok(Flow::Next)
            }
            "emit" => {
                let a = Args::parse_anywhere(&args, "t:").map_err(|e| format!("plugin emit: {}", e))?;
                let event = one(&a.pos)?;
                let pane = match a.get('t') {
                    Some(t) => Some(self.target_pane(cid, Some(t))?.2),
                    None => self.current(cid).2,
                };
                let mut params = Map::new();
                for kv in a.pos.iter().skip(1) {
                    let (k, v) = kv.split_once('=').ok_or_else(|| format!("plugin emit: expected KEY=VALUE, got {}", kv))?;
                    params.insert(k.to_string(), emit_value(v));
                }
                params.insert("synthetic".into(), true.into());
                let n = self.plugins_event(&event, pane, params);
                let note = if n == 0 { " (no running plugin subscribes to it)" } else { "" };
                self.output(cid, "plugin", vec![format!("{} sent to {} plugin(s){}", event, n, note)]);
                Ok(Flow::Next)
            }
            _ => Err(USAGE.into()),
        }
    }

    /// Turn a plugin off (persistent): stop it and undo its effects.
    fn plugin_disable(&mut self, id: u32) {
        let Some(i) = self.plugins.idx(id) else { return };
        self.plugins.disabled.insert(self.plugins.plugins[i].name.clone());
        self.plugins.save_disabled();
        if self.plugins.plugins[i].proc.is_some() {
            self.plugin_stop(id, After::Disable, "disabled");
        } else {
            self.plugin_unwind(id, "disabled");
            if let Some(i) = self.plugins.idx(id) {
                self.plugins.plugins[i].state = PState::Disabled;
            }
        }
    }

    fn plugin_enable(&mut self, id: u32) {
        let Some(i) = self.plugins.idx(id) else { return };
        let name = self.plugins.plugins[i].name.clone();
        self.plugins.disabled.remove(&name);
        self.plugins.save_disabled();
        match self.plugins.plugins[i].state.clone() {
            PState::Disabled => {
                self.plugins.plugins[i].state = PState::Exited;
                self.plugin_start(id);
            }
            PState::Stopping { since, termed, .. } => self.plugins.plugins[i].state = PState::Stopping { since, termed, then: After::Restart },
            _ => {}
        }
    }

    fn plugin_named(&self, name: &str) -> Result<usize, String> {
        self.plugins.find(name).ok_or_else(|| format!("no plugin named {} in {} (see: amux plugin list)", name, self.plugins.dir.display()))
    }

    fn plugin_wait_load(&mut self, cid: u32, ids: Vec<u32>) -> R {
        if let Some(ci) = self.client_idx(cid) {
            self.clients[ci].waiting = Some(Wait::PluginLoad { ids, deadline: Instant::now() + LOAD_TIMEOUT });
            return Ok(Flow::Wait);
        }
        Ok(Flow::Next)
    }

    fn plugin_table(&self) -> Vec<String> {
        if self.plugins.plugins.is_empty() {
            return vec![format!("no plugins in {}", self.plugins.dir.display()), "create one: amux plugin new NAME    guide: amux plugin guide".into()];
        }
        let cut = |s: String, n: usize| if s.chars().count() > n { format!("{}…", s.chars().take(n - 1).collect::<String>()) } else { s };
        let mut lines = vec![format!("{:<16} {:<10} {:<24} {:<22} {}", "NAME", "STATE", "COMMANDS", "EVENTS", "INFO")];
        for p in &self.plugins.plugins {
            let cmds = p.reg.commands.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(",");
            let evs = p.reg.events.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(",");
            let info = match &p.state {
                PState::Running | PState::Starting => {
                    let up = p.started.map(|t| ago(t.elapsed())).unwrap_or_default();
                    let mut s = format!("pid {}, up {}", p.pid().unwrap_or(0), up);
                    if p.restarts > 0 {
                        s.push_str(&format!(", {} restart(s)", p.restarts));
                    }
                    if !p.notes.is_empty() {
                        s.push_str(&format!(", {} rejected (amux plugin show {})", p.notes.len(), p.name));
                    }
                    s
                }
                PState::Disabled => format!("amux plugin enable {}", p.name),
                _ => p.last_error.clone(),
            };
            let cmds = if cmds.is_empty() { "-".to_string() } else { cmds };
            let evs = if evs.is_empty() { "-".to_string() } else { evs };
            lines.push(format!("{:<16} {:<10} {:<24} {:<22} {}", p.name, p.state.label(), cut(cmds, 24), cut(evs, 22), info));
        }
        lines
    }

    fn plugin_details(&self, i: usize) -> Vec<String> {
        let p = &self.plugins.plugins[i];
        let mut l = vec![format!("name      {}", p.name)];
        let mut st = p.state.label().to_string();
        if let (Some(pid), Some(t)) = (p.pid(), p.started) {
            st.push_str(&format!(" (pid {}, up {})", pid, ago(t.elapsed())));
        }
        l.push(format!("state     {}; loaded {}x, {} restart(s)", st, p.loads, p.restarts));
        l.push(format!("file      {}", p.path.display()));
        l.push(match &p.runner {
            Ok(a) => format!("runs      {}", a.join(" ")),
            Err(e) => format!("runs      (cannot run: {})", e),
        });
        for (n, h) in &p.reg.commands {
            l.push(if h.is_empty() { format!("command   {}", n) } else { format!("command   {} — {}", n, h) });
        }
        for k in &p.reg.keys {
            let cmd = k.cmds.iter().map(|c| c.join(" ")).collect::<Vec<_>>().join(" ; ");
            l.push(format!("key       {} → {}", k.label, cmd));
        }
        if !p.reg.events.is_empty() {
            l.push(format!("events    {}", p.reg.events.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
        }
        for a in &p.reg.agents {
            let cmd = self.agent_kinds.by_name(a).map(|k| k.def.cmd.clone()).unwrap_or_default();
            let replaces = if self.agent_kinds.is_builtin(a) { " (replaces the built-in definition)" } else { "" };
            l.push(format!("agent     {} — matches {}{}", a, cmd, replaces));
        }
        if !p.reg.status.is_empty() {
            l.push(format!("status    {}", p.reg.status));
        }
        for (pane, t) in &p.reg.annotations {
            l.push(format!("annotate  %{}: {}", pane, t));
        }
        if !p.reg.statuses.is_empty() {
            let mut v: Vec<String> = p.reg.statuses.iter().map(|s| format!("%{}", s)).collect();
            v.sort();
            l.push(format!("statuses  {}", v.join(" ")));
        }
        for n in &p.notes {
            l.push(format!("note      {}", n));
        }
        if p.dropped > 0 {
            l.push(format!("dropped   {} message(s): the plugin was not reading its input", p.dropped));
        }
        if !p.last_error.is_empty() {
            l.push(format!("error     {}", p.last_error));
        }
        l.push(format!("log       {}", self.plugins.log_path(&p.name).display()));
        l.push(format!("data      {}", self.plugins.data_dir.join(&p.name).display()));
        if matches!(p.state, PState::Failed | PState::Backoff { .. }) {
            l.extend(log_tail(&self.plugins.log_path(&p.name), 12).into_iter().map(|t| format!("  | {}", t)));
        }
        l
    }

    fn plugin_new(&mut self, cid: u32, args: &[String]) -> R {
        let a = Args::parse_anywhere(args, "l:").map_err(|e| format!("plugin new: {}", e))?;
        let name = a.pos.first().cloned().ok_or("usage: plugin new NAME [-l python|sh]")?;
        if !valid_name(&name) {
            return Err(format!("invalid plugin name {:?}: use letters, digits, '-' and '_'", name));
        }
        let (ext, template) = match a.get('l').unwrap_or("python") {
            "python" | "py" => ("py", TEMPLATE_PY),
            "sh" | "shell" => ("sh", TEMPLATE_SH),
            other => return Err(format!("plugin new: unknown language {} (python, sh)", other)),
        };
        let taken = std::fs::read_dir(&self.plugins.dir)
            .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).find(|f| !ignored(f) && stem_of(f) == name))
            .unwrap_or(None);
        if let Some(f) = taken {
            return Err(format!("plugin {} already exists: {}", name, self.plugins.dir.join(f).display()));
        }
        let path = self.plugins.dir.join(format!("{}.{}", name, ext));
        let text = template.replace("{{NAME}}", &name).replace("{{IDENT}}", &format!("cmd_{}", name.replace('-', "_")));
        std::fs::write(&path, text).map_err(|e| format!("{}: {}", path.display(), e))?;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
        self.plugins_rescan();
        let i = self.plugin_named(&name)?;
        let id = self.plugins.plugins[i].id;
        self.output(
            cid,
            "plugin",
            vec![format!("created {}", path.display()), format!("edit and save it to reload; try: amux {}   guide: amux plugin guide", name)],
        );
        self.plugin_wait_load(cid, vec![id])
    }

    fn plugin_install(&mut self, cid: u32, args: &[String]) -> R {
        let a = Args::parse_anywhere(args, "f").map_err(|e| format!("plugin install: {}", e))?;
        let src = a.pos.first().ok_or("usage: plugin install PATH [-f]")?;
        let mut src = PathBuf::from(util::expand_home(src));
        if src.is_relative() {
            let base = self.client_idx(cid).map(|ci| self.clients[ci].cwd.clone()).filter(|c| !c.is_empty()).unwrap_or_else(util::home);
            src = Path::new(&base).join(src);
        }
        let meta = std::fs::metadata(&src).map_err(|e| format!("{}: {}", src.display(), e))?;
        let fname = src.file_name().map(|f| f.to_string_lossy().into_owned()).ok_or("plugin install: bad path")?;
        if ignored(&fname) {
            return Err(format!("{} does not look like a plugin (hidden, backup or documentation file)", fname));
        }
        let name = if meta.is_dir() { fname.clone() } else { stem_of(&fname).to_string() };
        if !valid_name(&name) {
            return Err(format!("invalid plugin name {:?}: use letters, digits, '-' and '_'", name));
        }
        let dest = self.plugins.dir.join(&fname);
        if !util::same_file(&src, &dest) {
            if let Some(i) = self.plugins.find(&name) {
                if !a.has('f') {
                    return Err(format!("plugin {} is already installed ({}); add -f to replace it", name, self.plugins.plugins[i].path.display()));
                }
                let old = self.plugins.plugins[i].path.clone();
                self.plugin_trash(&old)?;
            }
            if meta.is_dir() {
                copy_dir(&src, &dest).map_err(|e| format!("copy {}: {}", src.display(), e))?;
            } else {
                std::fs::copy(&src, &dest).map_err(|e| format!("copy {}: {}", src.display(), e))?;
            }
        }
        self.plugins_rescan();
        let i = self.plugin_named(&name)?;
        let id = self.plugins.plugins[i].id;
        self.output(cid, "plugin", vec![format!("installed {} as {}", src.display(), dest.display())]);
        self.plugin_wait_load(cid, vec![id])
    }

    /// Move a plugin's file or directory out of the plugin directory.
    fn plugin_trash(&self, path: &Path) -> Result<PathBuf, String> {
        let trash = self.plugins.log_dir.join("removed");
        std::fs::create_dir_all(&trash).map_err(|e| format!("{}: {}", trash.display(), e))?;
        let fname = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        let dest = trash.join(format!("{}.{}", fname, util::now_ms()));
        if std::fs::rename(path, &dest).is_ok() {
            return Ok(dest);
        }
        // Different filesystem: copy, then delete.
        if path.is_dir() {
            copy_dir(path, &dest).and_then(|_| std::fs::remove_dir_all(path)).map_err(|e| format!("move {}: {}", path.display(), e))?;
        } else {
            std::fs::copy(path, &dest).and_then(|_| std::fs::remove_file(path)).map_err(|e| format!("move {}: {}", path.display(), e))?;
        }
        Ok(dest)
    }
}

// ----- plugin view -----------------------------------------------------------

/// What the lower part of the plugin view shows for the selected plugin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Show {
    Log,
    Trace,
    Details,
}

impl Show {
    fn next(self) -> Show {
        match self {
            Show::Log => Show::Trace,
            Show::Trace => Show::Details,
            Show::Details => Show::Log,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Show::Log => "log: stderr, print() output, tracebacks",
            Show::Trace => "protocol trace: → to the plugin, ← from it",
            Show::Details => "details",
        }
    }
}

/// The plugin view (`prefix P`, `amux plugin view`): every plugin and its
/// state, with a live log, protocol trace or details of the selected one.
#[derive(Clone, Debug)]
pub struct View {
    /// The selected plugin.
    pub sel: String,
    pub show: Show,
    /// The log (or trace, or details) fills the screen.
    pub zoom: bool,
    /// First row shown when zoomed; `None` follows the end.
    pub top: Option<usize>,
    /// Fingerprint of what was last drawn: redraw when it changes.
    pub seen: u64,
}

/// How a state or a line looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Normal,
    Good,
    Busy,
    Bad,
    Dim,
    Note,
    Out,
}

pub struct ViewRow {
    pub name: String,
    pub state: &'static str,
    pub tone: Tone,
    pub info: String,
}

impl Server {
    /// Plugins as the view lists them.
    pub fn plugin_view_rows(&self) -> Vec<ViewRow> {
        let mut rows: Vec<ViewRow> = self
            .plugins
            .plugins
            .iter()
            .map(|p| {
                let tone = match &p.state {
                    PState::Running if p.notes.is_empty() && p.dropped == 0 => Tone::Good,
                    PState::Running | PState::Starting | PState::Stopping { .. } | PState::Backoff { .. } => Tone::Busy,
                    PState::Failed => Tone::Bad,
                    PState::Exited | PState::Disabled => Tone::Dim,
                };
                let info = match &p.state {
                    PState::Disabled => "disabled (d enables it)".to_string(),
                    PState::Failed | PState::Backoff { .. } | PState::Exited => p.last_error.clone(),
                    _ => {
                        let mut parts = Vec::new();
                        if let (Some(pid), Some(t)) = (p.pid(), p.started) {
                            parts.push(format!("pid {} · up {}", pid, ago(t.elapsed())));
                        }
                        if p.restarts > 0 {
                            parts.push(format!("{} restart(s)", p.restarts));
                        }
                        if !p.notes.is_empty() {
                            parts.push(format!("{} rejected", p.notes.len()));
                        }
                        let adds = p.summary();
                        if !adds.is_empty() {
                            parts.push(adds);
                        }
                        parts.join(" · ")
                    }
                };
                ViewRow { name: p.name.clone(), state: p.state.label(), tone, info }
            })
            .collect();
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        rows
    }

    /// A few lines about a plugin, shown above its log.
    pub fn plugin_view_summary(&self, name: &str) -> Vec<(String, Tone)> {
        let Some(i) = self.plugins.find(name) else { return Vec::new() };
        let p = &self.plugins.plugins[i];
        let runner = match &p.runner {
            Ok(a) => a.first().map(|r| r.rsplit('/').next().unwrap_or(r).to_string()).unwrap_or_default(),
            Err(_) => "cannot run".into(),
        };
        let mut st = format!("{} · {} · loaded {}× · {} restart(s)", p.state.label(), runner, p.loads, p.restarts);
        if let (Some(pid), Some(t)) = (p.pid(), p.started) {
            st.push_str(&format!(" · pid {} · up {}", pid, ago(t.elapsed())));
        }
        let adds = p.summary();
        let mut l = vec![
            (format!("state  {}", st), Tone::Normal),
            (format!("file   {}", p.path.display()), Tone::Dim),
            (format!("adds   {}", if adds.is_empty() { "nothing (yet)".to_string() } else { adds }), Tone::Normal),
        ];
        if !p.last_error.is_empty() {
            l.push((format!("error  {}", p.last_error), if p.state == PState::Running { Tone::Dim } else { Tone::Bad }));
        }
        for n in &p.notes {
            l.push((format!("note   {}", n), Tone::Busy));
        }
        if p.dropped > 0 {
            l.push((format!("note   {} message(s) not delivered: the plugin was not reading its input", p.dropped), Tone::Busy));
        }
        l
    }

    /// The last `max` lines of a plugin's log, trace or details.
    pub fn plugin_view_lines(&self, name: &str, show: Show, max: usize) -> Vec<(String, Tone)> {
        let Some(i) = self.plugins.find(name) else { return Vec::new() };
        let mut out: Vec<(String, Tone)> = match show {
            Show::Log => log_lines(&self.plugins.log_path(name), max).into_iter().map(|l| (l.clone(), log_tone(&l))).collect(),
            Show::Trace => self.plugins.plugins[i]
                .trace
                .iter()
                .map(|t| {
                    let tone = match t.dir {
                        '→' => Tone::Out,
                        '·' => Tone::Note,
                        '✗' => Tone::Bad,
                        _ => Tone::Normal,
                    };
                    (t.show(), tone)
                })
                .collect(),
            Show::Details => self.plugin_details(i).into_iter().map(|l| (clean_text(&l), Tone::Normal)).collect(),
        };
        if out.is_empty() {
            let empty = match show {
                Show::Log => "(nothing logged yet: print() output and tracebacks show up here)",
                _ => "(no messages yet)",
            };
            out.push((empty.into(), Tone::Dim));
        }
        let skip = out.len().saturating_sub(max);
        out.drain(..skip);
        out
    }

    /// `plugin view [NAME]`: open (or close) the plugin view on the caller's
    /// terminal; from a shell, on the terminal last used with its session.
    fn plugin_view_open(&mut self, cid: u32, name: Option<String>) -> R {
        let Some(i) = self.target_attached_client(cid, self.current(cid).0).and_then(|t| self.client_idx(t)) else {
            return Err("plugin view needs a terminal attached to amux: press prefix P there (or use amux plugin list, show, logs, trace)".into());
        };
        if name.is_none() && matches!(self.clients[i].overlay, Some(Overlay::Plugins(_))) {
            self.clients[i].overlay = None;
        } else {
            // The named plugin, else the first that needs attention.
            let rows = self.plugin_view_rows();
            let first = rows.iter().find(|r| matches!(r.tone, Tone::Bad | Tone::Busy)).or(rows.first()).map(|r| r.name.clone());
            let sel = name.or(first).unwrap_or_default();
            self.clients[i].overlay = Some(Overlay::Plugins(View { sel, show: Show::Log, zoom: false, top: None, seen: 0 }));
        }
        self.clients[i].dirty = true;
        Ok(Flow::Next)
    }

    /// A key pressed in the plugin view.
    pub fn plugin_view_key(&mut self, cid: u32, mut v: View, k: Key) {
        let rows = self.plugin_view_rows();
        let idx = rows.iter().position(|r| r.name == v.sel).unwrap_or(0);
        if let Some(r) = rows.get(idx) {
            v.sel = r.name.clone();
        }
        let ch = |c: char| k == Key::ch(c);
        let (down, up, quit) = (k.is_nav_down(), k.is_nav_up(), k.is_quit());
        let id = self.plugins.find(&v.sel).map(|i| self.plugins.plugins[i].id);
        let mut keep = true;
        let mut then: Option<Vec<Vec<String>>> = None;
        if quit {
            if v.zoom {
                v.zoom = false;
                v.top = None;
            } else {
                keep = false;
            }
        } else if k.code == Code::Enter || ch('z') {
            v.zoom = !v.zoom;
            v.top = None;
        } else if ch('t') || k.code == Code::Tab {
            v.show = v.show.next();
            v.top = None;
        } else if v.zoom {
            let page = self.client_idx(cid).map_or(20, |i| self.clients[i].rows.saturating_sub(3) as usize).max(1);
            let width = self.client_idx(cid).map_or(80, |i| self.clients[i].cols.saturating_sub(2) as usize).max(10);
            let total: usize = self.plugin_view_lines(&v.sel, v.show, VIEW_LINES).iter().map(|(l, _)| wrap(l, width).len()).sum();
            let end = total.saturating_sub(page);
            let cur = v.top.unwrap_or(end).min(end);
            let top = k.scroll(cur, page, end).unwrap_or(cur);
            // Scrolled to the end: follow new output again.
            v.top = (top < end).then_some(top);
        } else if down || up {
            if !rows.is_empty() {
                let j = if down { (idx + 1).min(rows.len() - 1) } else { idx.saturating_sub(1) };
                v.sel = rows[j].name.clone();
            }
        } else if let Some(id) = id {
            let Some(pi) = self.plugins.idx(id) else { return };
            let name = self.plugins.plugins[pi].name.clone();
            if ch('r') {
                self.plugin_reload(id);
                self.show_message(cid, &format!("reloading {}…", name));
            } else if ch('d') {
                if self.plugins.disabled.contains(&name) {
                    self.plugin_enable(id);
                    self.show_message(cid, &format!("{} enabled", name));
                } else {
                    self.plugin_disable(id);
                    self.show_message(cid, &format!("{} disabled (d enables it again)", name));
                }
            } else if ch('e') {
                // Edit in a new window; saving reloads the plugin.
                let p = &self.plugins.plugins[pi];
                let editor = format!("${{VISUAL:-${{EDITOR:-vi}}}} {}", util::shell_quote(&p.path.to_string_lossy()));
                then = Some(vec![vec!["new-window".into(), "-n".into(), name, "-c".into(), p.cwd.display().to_string(), editor]]);
                keep = false;
            } else if ch('x') {
                self.open_prompt(cid, &format!("remove plugin {}? (y/n)", name), "", &format!("plugin remove {}", name), false, true);
            }
        }
        if let Some(i) = self.client_idx(cid) {
            self.clients[i].overlay = keep.then_some(Overlay::Plugins(v));
            self.clients[i].dirty = true;
        }
        if let Some(cmds) = then {
            self.run_binding(cid, cmds);
        }
    }

    fn plugin_view_fingerprint(&self, sel: &str) -> u64 {
        let mut h = Fnv::new();
        // Uptimes in the view change every second.
        h.add(&util::now_unix().to_le_bytes());
        for p in &self.plugins.plugins {
            h.add(p.name.as_bytes());
            h.add(p.state.label().as_bytes());
            h.add(p.last_error.as_bytes());
            for v in [p.trace_seq, p.loads as u64, p.restarts as u64, p.pid().unwrap_or(0) as u64, p.notes.len() as u64, p.dropped] {
                h.add(&v.to_le_bytes());
            }
        }
        if let Ok(m) = std::fs::metadata(self.plugins.log_path(sel)) {
            h.add(&m.len().to_le_bytes());
            h.add(&m.mtime_nsec().to_le_bytes());
        }
        h.0
    }

    /// Redraw open plugin views whose content changed (plugin state, new
    /// protocol messages or log output: plugins write their logs directly).
    pub fn plugins_view_poll(&mut self) {
        let now = Instant::now();
        if now < self.plugins.view_poll + VIEW_POLL {
            return;
        }
        self.plugins.view_poll = now;
        for i in 0..self.clients.len() {
            let Some(Overlay::Plugins(v)) = &self.clients[i].overlay else { continue };
            let fp = self.plugin_view_fingerprint(&v.sel.clone());
            if let Some(Overlay::Plugins(v)) = &mut self.clients[i].overlay
                && v.seen != fp
            {
                v.seen = fp;
                self.clients[i].dirty = true;
            }
        }
    }

    /// When open plugin views next need checking.
    pub fn plugins_view_deadline(&self) -> Option<Instant> {
        self.clients.iter().any(|c| matches!(c.overlay, Some(Overlay::Plugins(_)))).then_some(self.plugins.view_poll + VIEW_POLL)
    }
}

/// Text as a terminal would show it, for amux's own UI: escape sequences
/// removed, carriage-return overwrites applied, tabs expanded, other control
/// characters dropped.
pub fn clean_text(s: &str) -> String {
    let s = s.rsplit('\r').find(|seg| !seg.is_empty()).unwrap_or("");
    let mut out = String::with_capacity(s.len());
    let mut col = 0;
    let mut it = s.chars();
    while let Some(c) = it.next() {
        match c {
            '\x1b' => match it.next() {
                // CSI: parameters, then a final byte.
                Some('[') => {
                    for n in it.by_ref() {
                        if ('\x40'..='\x7e').contains(&n) {
                            break;
                        }
                    }
                }
                // OSC, DCS, APC, PM: up to BEL or ESC \.
                Some(']' | 'P' | '_' | '^') => {
                    while let Some(n) = it.next() {
                        if n == '\x07' {
                            break;
                        }
                        if n == '\x1b' {
                            it.next();
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\t' => {
                let pad = 4 - col % 4;
                out.extend(std::iter::repeat_n(' ', pad));
                col += pad;
            }
            c if c.is_control() => {}
            c => {
                out.push(c);
                col += 1;
            }
        }
    }
    out
}

/// Split a display line into rows of at most `width` columns.
pub fn wrap(line: &str, width: usize) -> Vec<String> {
    let mut rows = vec![String::new()];
    let mut col = 0;
    for c in line.chars() {
        let w = crate::cell::char_width(c).max(1);
        if col + w > width && col > 0 {
            rows.push(String::new());
            col = 0;
        }
        if let Some(r) = rows.last_mut() {
            r.push(c);
        }
        col += w;
    }
    rows
}

/// The end of a plugin's log, as display lines.
fn log_lines(path: &Path, max: usize) -> Vec<String> {
    let lines = read_tail(path, (max as u64 * 512).clamp(16 * 1024, 256 * 1024));
    lines[lines.len().saturating_sub(max)..].iter().map(|l| clean_text(l)).collect()
}

/// How a log line looks: start markers, amux's own notes and errors stand out.
fn log_tone(l: &str) -> Tone {
    if l.starts_with("--- ") {
        Tone::Note
    } else if l.starts_with("[amux ") || l.starts_with("[amux]") {
        Tone::Busy
    } else if ["Traceback", "Error", "Exception", "error:", "panicked", "FATAL", "fatal:"].iter().any(|w| l.contains(w)) {
        Tone::Bad
    } else {
        Tone::Normal
    }
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)?.flatten() {
        let name = e.file_name();
        let n = name.to_string_lossy();
        if n.starts_with('.') || SKIP_DIRS.contains(&n.as_ref()) {
            continue;
        }
        let (from, to) = (e.path(), dst.join(&name));
        if std::fs::metadata(&from)?.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

fn ago(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{}s", s)
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h{}m", s / 3600, s % 3600 / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("amux-plugin-unit-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(dir: &Path, name: &str, text: &str, mode: u32) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, text).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        p
    }

    #[test]
    fn ignores_non_plugins() {
        for n in [".hidden.py", "x.py~", "#x.py#", "README.md", "notes.txt", "cfg.json", "x.py.swp", "__pycache__", "LICENSE"] {
            assert!(ignored(n), "{n}");
        }
        for n in ["hello.py", "hello", "run.sh", "my-tool.js"] {
            assert!(!ignored(n), "{n}");
        }
        assert!(valid_name("my-plugin_2") && !valid_name("my plugin") && !valid_name("-x") && !valid_name(""));
    }

    #[test]
    fn picks_a_runner() {
        let d = tmpdir("runner");
        let run = |p: &Path| runner_for_file(p, &std::fs::metadata(p).unwrap());
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let a = write(&d, "a.py", "print(1)\n", 0o644);
        assert_eq!(run(&a), Ok(vec!["python3".to_string(), s(&a)]));
        let b = write(&d, "b", "#!/usr/bin/env python3\nprint(1)\n", 0o644);
        assert_eq!(run(&b), Ok(vec!["/usr/bin/env".to_string(), "python3".to_string(), s(&b)]));
        let c = write(&d, "c", "#!/bin/sh\necho\n", 0o755);
        assert_eq!(run(&c), Ok(vec![s(&c)]));
        let u = write(&d, "u.py", "#!/usr/bin/env -S uv run --script\n", 0o644);
        assert_eq!(run(&u), Ok(vec!["/usr/bin/env".to_string(), "-S uv run --script".to_string(), s(&u)]));
        let e = write(&d, "e", "plain text\n", 0o644);
        assert!(run(&e).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn discovers_files_and_directories() {
        let d = tmpdir("discover");
        write(&d, "alpha.py", "x\n", 0o644);
        write(&d, "alpha.sh", "x\n", 0o644);
        write(&d, "beta.sh", "x\n", 0o644);
        write(&d, ".hidden.py", "x\n", 0o644);
        write(&d, "README.md", "x\n", 0o644);
        std::fs::create_dir_all(d.join("gamma")).unwrap();
        write(&d.join("gamma"), "main.py", "x\n", 0o644);
        std::fs::create_dir_all(d.join("delta")).unwrap();
        write(&d.join("delta"), "notes.txt", "x\n", 0o644);
        write(&d, "bad name.py", "x\n", 0o644);
        let found = discover(&d);
        let names: Vec<&str> = found.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "bad_name", "beta", "delta", "gamma"]);
        assert_eq!(found[0].runner.as_ref().unwrap()[0], "python3", "first of alpha.py/alpha.sh wins");
        assert!(found[1].runner.is_err(), "invalid names are reported");
        assert!(found[3].runner.as_ref().unwrap_err().contains("no entry point"));
        assert_eq!(found[4].cwd, d.join("gamma"));
        assert!(found[4].runner.as_ref().unwrap()[1].ends_with("gamma/main.py"));
        // Any change to a directory plugin's files changes its fingerprint.
        let before = found[4].fingerprint;
        std::thread::sleep(Duration::from_millis(20));
        write(&d.join("gamma"), "helper.py", "y\n", 0o644);
        assert_ne!(discover(&d)[4].fingerprint, before);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn options_may_follow_positionals() {
        let v = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
        let a = Args::parse_anywhere(&v("shy -l sh"), "l:").unwrap();
        assert_eq!((a.get('l'), a.pos.clone()), (Some("sh"), v("shy")));
        let a = Args::parse_anywhere(&v("agent-state -t %0 state=waiting n=-5"), "t:").unwrap();
        assert_eq!((a.get('t'), a.pos.clone()), (Some("%0"), v("agent-state state=waiting n=-5")));
        let a = Args::parse_anywhere(&v("x.py -f"), "f").unwrap();
        assert!(a.has('f') && a.pos == v("x.py"));
    }

    #[test]
    fn cleans_terminal_text_for_display() {
        assert_eq!(clean_text("\x1b[1;31mred\x1b[0m plain"), "red plain");
        assert_eq!(clean_text("\x1b]0;title\x07after"), "after");
        assert_eq!(clean_text("\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\"), "link");
        assert_eq!(clean_text("10%\r20%\r"), "20%");
        assert_eq!(clean_text("a\tb"), "a   b");
        assert_eq!(clean_text("bell\x07 nul\x00"), "bell nul");
        assert_eq!(wrap("abcdef", 4), vec!["abcd", "ef"]);
        assert_eq!(wrap("日本語", 4), vec!["日本", "語"]);
        assert_eq!(wrap("", 4), vec![""]);
    }

    #[test]
    fn decodes_results_and_values() {
        assert_eq!(result_lines(Some(&json!("a\nb"))), vec!["a", "b"]);
        assert_eq!(result_lines(Some(&json!(["x", 1]))), vec!["x", "1"]);
        assert_eq!(result_lines(Some(&json!({"output": "o"}))), vec!["o"]);
        assert!(result_lines(Some(&Value::Null)).is_empty());
        assert_eq!(emit_value("3"), json!(3));
        assert_eq!(emit_value("true"), json!(true));
        assert_eq!(emit_value("waiting"), json!("waiting"));
    }
}
