//! The amux server: owns sessions/windows/layouts, talks to pane keepers and
//! clients, renders, and persists its state after every change.
//!
//! Crash-only design: the server never owns a PTY. If it panics or is killed,
//! the supervisor restarts it; it reloads `state.json`, reconnects to every
//! live keeper (getting a full screen back), resurrects panes whose keeper
//! died, adopts orphans, and clients reconnect on their own.

pub mod exec;
pub mod input;
pub mod migrate;
pub mod plugins;
pub mod render;

use crate::agents::{self, State as AState, Tracker};
use crate::cell::*;
use crate::keeper::{self, Spec};
use crate::keys;
use crate::model::{Node, PaneMeta, Rect, Session, State, Window};
use crate::options::{Options, Scope};
use crate::paths::Paths;
use crate::proto::{self, Conn, Dec, Enc, c2s, ev, k2s, s2c, s2k};
use crate::term::Modes;
use crate::{format, logf, pty, util};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{Duration, Instant};

pub const MIN_RENDER_INTERVAL: Duration = Duration::from_millis(8);

/// Server-side mirror of a pane's visible screen. Rows are stored sparsely
/// (trailing empty cells trimmed), so mostly-empty panes cost little.
#[derive(Default)]
pub struct Mirror {
    pub cols: usize,
    pub rows: usize,
    grid: Vec<Vec<Cell>>,
    pub wrapped: Vec<bool>,
    pub clusters: Clusters,
    pub cx: usize,
    pub cy: usize,
    pub cursor_visible: bool,
    pub modes: Modes,
    pub title: String,
}

impl Mirror {
    pub fn apply(&mut self, f: keeper::Frame) {
        if f.cols != self.cols || f.rows != self.rows {
            self.cols = f.cols;
            self.rows = f.rows;
            self.grid = vec![Vec::new(); f.rows];
            self.wrapped = vec![false; f.rows];
        }
        if f.full {
            self.clusters.clear();
        }
        for (id, s) in &f.clusters {
            self.clusters.set(*id, s);
        }
        for (y, w, mut cells) in f.lines {
            let mut n = cells.len().min(self.cols);
            while n > 0 && cells[n - 1] == Cell::EMPTY {
                n -= 1;
            }
            cells.truncate(n);
            self.grid[y] = cells;
            self.wrapped[y] = w;
        }
        self.cx = f.cx.min(self.cols.saturating_sub(1));
        self.cy = f.cy.min(self.rows.saturating_sub(1));
        self.cursor_visible = f.cursor_visible;
        self.modes = f.modes;
        self.title = f.title;
    }

    /// Row `y` without its trailing empty cells.
    #[inline]
    pub fn row(&self, y: usize) -> &[Cell] {
        self.grid.get(y).map(|r| r.as_slice()).unwrap_or(&[])
    }

    #[inline]
    pub fn cell(&self, y: usize, x: usize) -> Cell {
        self.row(y).get(x).copied().unwrap_or(Cell::EMPTY)
    }

    pub fn row_text(&self, y: usize) -> String {
        let mut s = String::new();
        for c in self.row(y) {
            push_cell_text(&mut s, c, &self.clusters);
        }
        let t = s.trim_end().len();
        s.truncate(t);
        s
    }

    pub fn lines(&self) -> Vec<String> {
        (0..self.rows).map(|y| self.row_text(y)).collect()
    }
}

pub struct Pane {
    pub meta: PaneMeta,
    pub conn: Option<Conn>,
    pub mirror: Mirror,
    pub exited: Option<i32>,
    pub in_copy: bool,
    pub keeper_pid: u32,
    pub child_pid: u32,
    pub fg_pid: u32,
    pub agent: Tracker,
    pub dirty_since_snapshot: bool,
    pub recoveries: u32,
    pub size: (u16, u16),
    pub ready: bool,
}

pub struct Prompt {
    pub label: String,
    pub text: String,
    pub cursor: usize,
    /// Command template; `%%`/`%1` replaced by the input.
    pub template: String,
    pub single_key: bool,
    pub confirm: bool,
    pub history_pos: Option<usize>,
}

pub enum Overlay {
    Agents { sel: usize, tiles: bool, filter: String },
    Tree { sel: usize, expanded: std::collections::HashSet<u32>, preview: bool },
    View { title: String, lines: Vec<String>, top: usize },
    Panes { until: Instant },
    Plugins(plugins::View),
}

#[derive(Clone)]
pub struct TreeItem {
    pub label: String,
    pub session: u32,
    pub window: Option<u32>,
}

/// Optional start-up state for a new pane's keeper.
#[derive(Default)]
pub struct Start {
    /// Snapshot file whose history is restored first.
    pub snapshot_in: Option<String>,
    /// Text shown before the child starts.
    pub banner: Option<String>,
    /// Keys typed into the child shortly after it starts.
    pub type_in: Option<String>,
    /// File of terminal output replayed first, then deleted.
    pub feed_in: Option<String>,
    /// Bridged tmux pane process (pid, start ticks).
    pub proc_of: Option<(u32, u64)>,
}

pub enum Wait {
    /// `argv` is kept so the capture can be retried if the keeper is replaced.
    Capture {
        pane: u32,
        req: u32,
        buffer: Option<String>,
        print: bool,
        argv: Vec<String>,
        deadline: Instant,
    },
    Exit {
        pane: u32,
        deadline: Option<Instant>,
        print_output: bool,
    },
    Idle {
        pane: u32,
        quiet: Duration,
        deadline: Option<Instant>,
    },
    Pattern {
        pane: u32,
        re: regex_lite::Regex,
        deadline: Option<Instant>,
    },
    Agent {
        pane: u32,
        states: Vec<AState>,
        deadline: Option<Instant>,
    },
    Job {
        id: u32,
    },
    Events,
    /// A plugin is answering a `command` request.
    Plugin {
        plugin: u32,
        req: u64,
        deadline: Instant,
    },
    /// `plugin reload/new/enable/install`: until these plugins are up or failed.
    PluginLoad {
        ids: Vec<u32>,
        deadline: Instant,
    },
    /// Re-run `argv` once in-progress plugin (re)loads finish.
    PluginSettle {
        argv: Vec<String>,
        deadline: Instant,
    },
}

pub struct Client {
    pub id: u32,
    pub conn: Conn,
    pub cols: u16,
    pub rows: u16,
    pub term: String,
    pub truecolor: bool,
    pub env: Vec<(String, String)>,
    pub cwd: String,
    pub pid: u32,
    pub attached: Option<u32>,
    pub parser: keys::Parser,
    pub key_table: String,
    pub repeat_until: Option<Instant>,
    pub esc_deadline: Option<Instant>,
    pub message: Option<(String, Instant)>,
    pub prompt: Option<Prompt>,
    pub overlay: Option<Overlay>,
    pub front: Vec<Cell>,
    pub front_size: (u16, u16),
    pub status_cache: Option<render::StatusCache>,
    /// The previous frame's canvas allocation, reused by the next render.
    pub spare: Vec<Cell>,
    pub clusters: Clusters,
    pub cursor: (u16, u16, bool, u8),
    pub full_redraw: bool,
    pub dirty: bool,
    pub last_render: Instant,
    pub queue: VecDeque<Vec<String>>,
    pub waiting: Option<Wait>,
    pub cli: bool,
    pub exit_code: i32,
    pub drag: Option<input::Drag>,
    pub last_session: Option<u32>,
    pub outer_mouse: bool,
    pub outer_focus: bool,
    pub activity: Instant,
    pub caller_pane: Option<u32>,
    pub closing: bool,
    pub title: String,
    pub events_json: bool,
    /// Internal client executing a plugin's `run` requests.
    pub plugin: Option<u32>,
    /// Output frames captured instead of sent (plugin clients).
    pub sink: Option<Vec<(u8, Vec<u8>)>>,
    /// The front command is a retry after waiting for plugins to settle.
    pub settle_retry: bool,
}

impl Client {
    fn out(&mut self, ty: u8, b: &[u8]) {
        if let Some(sink) = self.sink.as_mut() {
            sink.push((ty, b.to_vec()));
            return;
        }
        self.conn.send(ty, b);
    }
    pub fn print(&mut self, s: &str) {
        let mut t = s.to_string();
        if !t.ends_with('\n') {
            t.push('\n');
        }
        self.out(s2c::STDOUT, t.as_bytes());
    }
    pub fn eprint(&mut self, s: &str) {
        let mut t = s.to_string();
        if !t.ends_with('\n') {
            t.push('\n');
        }
        self.out(s2c::STDERR, t.as_bytes());
    }
}

pub struct Job {
    pub id: u32,
    pub pid: u32,
    pub out: OwnedFd,
    pub buf: Vec<u8>,
    pub client: Option<u32>,
    pub done: Option<i32>,
    pub eof: bool,
    pub show: bool,
    pub on_done: Vec<Vec<String>>,
    pub on_fail: Vec<Vec<String>>,
}

pub struct Server {
    pub paths: Paths,
    pub sessions: Vec<Session>,
    pub windows: HashMap<u32, Window>,
    pub panes: HashMap<u32, Pane>,
    pub next_session: u32,
    pub next_window: u32,
    pub next_pane: u32,
    pub clients: Vec<Client>,
    pub next_client: u32,
    pub options: Options,
    pub keys: input::KeyTables,
    pub buffers: Vec<(String, String)>,
    pub next_buffer: u32,
    pub env: Vec<(String, String)>,
    pub messages: VecDeque<String>,
    pub jobs: Vec<Job>,
    pub next_job: u32,
    pub next_req: u32,
    pub listener: UnixListener,
    pub state_dirty: bool,
    pub meta_dirty_at: Option<Instant>,
    pub start_time: u64,
    pub config_files: Vec<String>,
    pub shell_cache: std::cell::RefCell<HashMap<String, (String, Instant)>>,
    /// Bumped whenever the status line may need re-expanding.
    pub status_gen: u64,
    pub prompt_history: Vec<String>,
    pub quit: Option<i32>,
    pub last_snapshot: Instant,
    pub heartbeat: Option<OwnedFd>,
    pub last_heartbeat: Instant,
    pub last_status_tick: Instant,
    pub last_agent_tick: Instant,
    pub recovered_banner: Option<String>,
    pub restarts: u32,
    pub wait_channels: HashMap<String, Vec<u32>>,
    pub hooks: HashMap<String, Vec<String>>,
    pub plugins: plugins::Host,
    /// Agent definitions: built in, plus those registered by plugins.
    pub agent_kinds: agents::Kinds,
}

pub fn exe_path() -> String {
    if let Ok(p) = std::env::current_exe() {
        let s = p.to_string_lossy();
        let clean = s.strip_suffix(" (deleted)").unwrap_or(&s);
        if std::path::Path::new(clean).is_file() {
            return clean.to_string();
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let cand = dir.join("amux");
            if cand.is_file() {
                return cand.to_string_lossy().into_owned();
            }
        }
    }
    if std::path::Path::new("/proc/self/exe").exists() { "/proc/self/exe".into() } else { "amux".into() }
}

fn take_lock(path: &std::path::Path) -> io::Result<OwnedFd> {
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path)?;
    let fd: OwnedFd = f.into();
    // SAFETY: flock on our own descriptor.
    let r = unsafe { libc::flock(fd.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if r != 0 {
        return Err(io::Error::new(io::ErrorKind::AddrInUse, "another server is running"));
    }
    Ok(fd)
}

pub fn main(name: &str, config: Option<String>) -> i32 {
    let paths = Paths::new(name);
    if let Err(e) = paths.ensure() {
        eprintln!("amux: cannot create {}: {}", paths.run.display(), e);
        return 1;
    }
    util::log_init(&paths.log_file(), "server");
    util::install_panic_log();
    let _lock = match take_lock(&paths.lock()) {
        Ok(l) => l,
        Err(e) => {
            logf!("not starting: {}", e);
            return 0;
        }
    };
    util::ignore_signal(libc::SIGPIPE);
    util::ignore_signal(libc::SIGHUP);
    util::ignore_signal(libc::SIGINT);
    util::ignore_signal(libc::SIGTSTP);
    let sigs = match util::Signals::install(&[libc::SIGCHLD, libc::SIGTERM, libc::SIGUSR1]) {
        Ok(s) => s,
        Err(e) => {
            logf!("signals: {}", e);
            return 1;
        }
    };
    let sock = paths.server_sock();
    let _ = std::fs::remove_file(&sock);
    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => {
            logf!("bind {}: {}", sock.display(), e);
            return 1;
        }
    };
    let _ = listener.set_nonblocking(true);
    let heartbeat = {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: probing fd 4 (heartbeat pipe from the supervisor).
        if unsafe { libc::fstat(4, &mut st) } == 0 && (st.st_mode & libc::S_IFMT) == libc::S_IFIFO {
            let _ = util::set_cloexec(4, true);
            // SAFETY: we own fd 4 from here on.
            Some(unsafe { OwnedFd::from_raw_fd(4) })
        } else {
            None
        }
    };
    let restarts: u32 = std::env::var("AMUX_RESTARTS").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| {
            matches!(k.as_str(), "AMUX_RUNTIME_DIR" | "AMUX_STATE_DIR" | "AMUX_PLUGIN_DIR")
                || (!k.starts_with("AMUX_") && k != "AMUX" && k != "TMUX" && k != "TMUX_PANE")
        })
        .collect();
    env.sort();
    let plugin_host = plugins::Host::new(&paths);
    let mut srv = Server {
        paths,
        sessions: Vec::new(),
        windows: HashMap::new(),
        panes: HashMap::new(),
        next_session: 0,
        next_window: 0,
        next_pane: 0,
        clients: Vec::new(),
        next_client: 1,
        options: Options::new(),
        keys: input::KeyTables::default_tables(),
        buffers: Vec::new(),
        next_buffer: 0,
        env,
        messages: VecDeque::new(),
        jobs: Vec::new(),
        next_job: 1,
        next_req: 1,
        listener,
        state_dirty: false,
        meta_dirty_at: None,
        start_time: util::now_unix(),
        config_files: Vec::new(),
        shell_cache: std::cell::RefCell::new(HashMap::new()),
        status_gen: 0,
        prompt_history: Vec::new(),
        quit: None,
        last_snapshot: Instant::now(),
        heartbeat,
        last_heartbeat: Instant::now(),
        last_status_tick: Instant::now(),
        last_agent_tick: Instant::now(),
        recovered_banner: None,
        restarts,
        wait_channels: HashMap::new(),
        hooks: HashMap::new(),
        plugins: plugin_host,
        agent_kinds: agents::Kinds::new(),
    };
    srv.install_shim();
    srv.load_config(config.clone());
    srv.recover();
    srv.plugins.booted = true;
    srv.plugins_init();
    logf!("server ready (restarts={}, sessions={}, panes={})", restarts, srv.sessions.len(), srv.panes.len());
    let unsupervised = srv.heartbeat.is_none();
    let code = srv.run(&sigs);
    let _ = std::fs::remove_file(srv.paths.server_sock());
    drop(_lock);
    if unsupervised && code == 76 {
        let mut argv = vec![exe_path(), "__supervisor".into(), name.to_string()];
        if let Some(cfg) = config {
            argv.push(cfg);
        }
        let _ = pty::spawn_daemon(&argv, None, Some(&srv.paths.log_file()));
        return 0;
    }
    code
}

impl Server {
    pub fn scope(&self, session: Option<u32>, window: Option<u32>) -> Scope {
        Scope { session, window }
    }

    pub fn opt(&self, session: Option<u32>, window: Option<u32>, key: &str) -> String {
        self.options.get(&self.scope(session, window), key)
    }

    pub fn gopt(&self, key: &str) -> String {
        self.options.global.get(key).cloned().unwrap_or_default()
    }

    pub fn gflag(&self, key: &str) -> bool {
        matches!(self.gopt(key).as_str(), "on" | "1" | "yes" | "true")
    }

    pub fn gnum(&self, key: &str, def: i64) -> i64 {
        self.gopt(key).trim().parse().unwrap_or(def)
    }

    fn install_shim(&self) {
        // `tmux` inside run-shell/if-shell resolves to amux talking to this server.
        // `amux` there too, for plugins (amux may not be on PATH).
        let dir = self.paths.shim_dir();
        let _ = std::fs::create_dir_all(&dir);
        let target = exe_path();
        for name in ["tmux", "amux"] {
            let link = dir.join(name);
            let _ = std::fs::remove_file(&link);
            let _ = std::os::unix::fs::symlink(&target, &link);
        }
    }

    fn load_config(&mut self, config: Option<String>) {
        let files: Vec<String> = match config {
            Some(f) => vec![f],
            None => {
                let mut v = vec![format!("{}/.tmux.conf", util::home())];
                let xdg = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| format!("{}/.config", util::home()));
                v.push(format!("{}/tmux/tmux.conf", xdg));
                v.push(format!("{}/amux/amux.conf", xdg));
                v
            }
        };
        for f in files {
            if std::path::Path::new(&f).exists() {
                let errs = self.source_file(&f, None);
                for e in errs {
                    logf!("config: {}", e);
                    self.messages.push_back(e);
                }
                self.config_files.push(f);
            }
        }
    }

    // ----- persistence -------------------------------------------------

    pub fn save_state(&mut self) {
        let _t = util::prof::time(util::prof::SAVE);
        self.state_dirty = false;
        let st = State {
            version: 1,
            next_session: self.next_session,
            next_window: self.next_window,
            next_pane: self.next_pane,
            sessions: self.sessions.clone(),
            windows: self.windows.values().cloned().collect(),
            panes: self.panes.values().map(|p| p.meta.clone()).collect(),
            saved_at: util::now_unix(),
        };
        match serde_json::to_vec_pretty(&st) {
            Ok(b) => {
                if let Err(e) = util::write_atomic(&self.paths.state_file(), &b) {
                    logf!("save state: {}", e);
                }
            }
            Err(e) => logf!("serialize state: {}", e),
        }
    }

    pub fn mark_dirty(&mut self) {
        self.state_dirty = true;
    }

    /// Rebuild from persisted state and live keepers.
    fn recover(&mut self) {
        let st: Option<State> = std::fs::read(self.paths.state_file()).ok().and_then(|b| serde_json::from_slice(&b).ok());
        let mut live = 0;
        let mut resurrected = 0;
        if let Some(st) = st {
            self.next_session = st.next_session;
            self.next_window = st.next_window;
            self.next_pane = st.next_pane;
            self.sessions = st.sessions;
            for w in st.windows {
                self.windows.insert(w.id, w);
            }
            let in_layout: std::collections::HashSet<u32> = self.windows.values().flat_map(|w| w.layout.panes()).collect();
            for meta in st.panes {
                if !in_layout.contains(&meta.id) {
                    continue;
                }
                let id = meta.id;
                let size = self.pane_size_hint(id);
                match UnixStream::connect(self.paths.keeper_sock(id)) {
                    Ok(s) => {
                        if let Ok(conn) = Conn::new(s) {
                            let mut p = Pane::new(meta, Some(conn), size, &self.agent_kinds);
                            p.send_hello();
                            self.panes.insert(id, p);
                            live += 1;
                        }
                    }
                    Err(_) => {
                        if self.resurrect(meta, size).is_ok() {
                            resurrected += 1;
                        }
                    }
                }
            }
            // Drop dangling layout references.
            let known: Vec<u32> = self.panes.keys().copied().collect();
            let wins: Vec<u32> = self.windows.keys().copied().collect();
            for wid in wins {
                let w = self.windows.get_mut(&wid).unwrap();
                for p in w.layout.panes() {
                    if !known.contains(&p) {
                        w.layout.remove(p);
                    }
                }
                let panes = w.layout.panes();
                if panes.iter().all(|p| !known.contains(p)) {
                    self.windows.remove(&wid);
                    for s in &mut self.sessions {
                        s.windows.retain(|(_, w)| *w != wid);
                    }
                } else if !panes.contains(&w.active) {
                    w.active = panes[0];
                }
            }
            self.sessions.retain(|s| !s.windows.is_empty());
            for s in &mut self.sessions {
                if !s.windows.iter().any(|(_, w)| *w == s.active) {
                    s.active = s.windows[0].1;
                }
            }
        }
        // Adopt keepers that are not in the state (created just before a crash).
        let mut orphans = Vec::new();
        if let Ok(rd) = std::fs::read_dir(self.paths.keeper_dir()) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if let Some(id) = name.strip_suffix(".sock").and_then(|s| s.parse::<u32>().ok())
                    && !self.panes.contains_key(&id)
                {
                    match UnixStream::connect(e.path()) {
                        Ok(s) => orphans.push((id, s)),
                        Err(_) => {
                            let _ = std::fs::remove_file(e.path());
                        }
                    }
                }
            }
        }
        if !orphans.is_empty() {
            let sid = self.new_session_shell("recovered");
            for (id, s) in orphans {
                if let Ok(conn) = Conn::new(s) {
                    let meta = PaneMeta { id, created: util::now_unix(), ..Default::default() };
                    let mut p = Pane::new(meta, Some(conn), (80, 23), &self.agent_kinds);
                    p.send_hello();
                    self.panes.insert(id, p);
                    self.next_pane = self.next_pane.max(id + 1);
                    let wid = self.next_window;
                    self.next_window += 1;
                    let win = Window {
                        id: wid,
                        name: "recovered".into(),
                        auto_name: true,
                        layout: Node::pane(id, 80),
                        active: id,
                        last: None,
                        zoomed: false,
                        sx: 80,
                        sy: 23,
                        bell: false,
                        activity: false,
                    };
                    self.windows.insert(wid, win);
                    let s = self.sessions.iter_mut().find(|s| s.id == sid).unwrap();
                    let idx = s.next_free_index(0);
                    s.insert(idx, wid);
                    s.active = wid;
                }
            }
            logf!("adopted orphan keepers into session 'recovered'");
        }
        if live + resurrected > 0 || !self.sessions.is_empty() {
            self.recovered_banner = Some(format!("amux recovered: {} live pane(s), {} resurrected (server restart #{})", live, resurrected, self.restarts));
        }
        self.save_state();
    }

    fn new_session_shell(&mut self, name: &str) -> u32 {
        let id = self.next_session;
        self.next_session += 1;
        let mut nm = name.to_string();
        let mut k = 1;
        while self.sessions.iter().any(|s| s.name == nm) {
            nm = format!("{}-{}", name, k);
            k += 1;
        }
        self.sessions.push(Session {
            id,
            name: nm,
            windows: vec![],
            active: 0,
            last: None,
            created: util::now_unix(),
            activity: util::now_unix(),
            cwd: util::home(),
        });
        id
    }

    fn pane_size_hint(&self, id: u32) -> (u16, u16) {
        for w in self.windows.values().filter(|w| w.layout.contains(id)) {
            let mut v = Vec::new();
            w.layout.layout(Rect { x: 0, y: 0, w: w.sx, h: w.sy }, &mut v);
            if let Some((_, r)) = v.into_iter().find(|(p, _)| *p == id) {
                return (r.w.max(2), r.h.max(1));
            }
        }
        (80, 24)
    }

    /// Format an agent's resume command according to `agent-resume` (`typed` / `run` / `off`).
    pub fn resume_input(&self, resume: Option<String>) -> Option<String> {
        let r = resume?;
        match self.gopt("agent-resume").as_str() {
            "run" => Some(format!("{}\r", r)),
            "typed" => Some(r),
            _ => None,
        }
    }

    /// Respawn a pane whose keeper is gone (e.g. after a reboot) in its last
    /// directory with its saved scrollback, and pre-type the agent resume
    /// command if one is known.
    fn resurrect(&mut self, mut meta: PaneMeta, size: (u16, u16)) -> io::Result<()> {
        let snap = self.paths.snap_file(meta.id);
        let snap_in = snap.exists().then(|| snap.to_string_lossy().into_owned());
        let when = format::strftime("%Y-%m-%d %H:%M");
        let banner = format!("\x1b[2m── amux: restored {} (process was not running) ──\x1b[0m\r\n", when);
        let resume = meta.agent.as_ref().and_then(|a| meta.agent_resume.clone().or_else(|| self.agent_kinds.by_name(a).and_then(|k| k.def.resume.clone())));
        let type_in = self.resume_input(resume);
        // Was the agent the pane's own command? (Plugin definitions are not
        // loaded yet: also compare with the agent's recorded command line.)
        let line = meta.argv.join(" ");
        let ran_it = agents::runs(&meta.fg_cmd, agents::program(&line));
        if type_in.is_some() && (self.agent_kinds.detect(&line).is_some() || ran_it) {
            // Resume it from a shell instead of starting a fresh conversation.
            meta.argv.clear();
        }
        let id = meta.id;
        self.spawn_keeper(meta, None, size, snap_in, Some(banner), type_in)?;
        logf!("resurrected pane {}", id);
        Ok(())
    }

    // ----- panes / keepers ---------------------------------------------

    pub fn pane_env(&self, session: Option<u32>, pane: u32) -> Vec<(String, String)> {
        let mut env: Vec<(String, String)> = self.env.clone();
        let set = |env: &mut Vec<(String, String)>, k: &str, v: String| {
            env.retain(|(a, _)| a != k);
            env.push((k.to_string(), v));
        };
        set(&mut env, "TERM", self.gopt("default-terminal"));
        let sock = self.paths.server_sock().to_string_lossy().into_owned();
        let sid = session.unwrap_or(0);
        set(&mut env, "AMUX", format!("{},{},{}", sock, std::process::id(), sid));
        set(&mut env, "AMUX_PANE", format!("%{}", pane));
        set(&mut env, "AMUX_SERVER", self.paths.name.clone());
        if self.gflag("tmux-env") {
            set(&mut env, "TMUX", format!("{},{},{}", sock, std::process::id(), sid));
            set(&mut env, "TMUX_PANE", format!("%{}", pane));
        } else {
            env.retain(|(k, _)| k != "TMUX" && k != "TMUX_PANE");
        }
        env
    }

    /// Spawn a keeper for `meta.id` (meta.argv empty => login shell).
    pub fn spawn_keeper(
        &mut self,
        meta: PaneMeta,
        session: Option<u32>,
        size: (u16, u16),
        snapshot_in: Option<String>,
        banner: Option<String>,
        type_in: Option<String>,
    ) -> io::Result<u32> {
        self.spawn_keeper_start(meta, session, size, Start { snapshot_in, banner, type_in, ..Default::default() })
    }

    /// `spawn_keeper` with every start-up option.
    pub fn spawn_keeper_start(&mut self, mut meta: PaneMeta, session: Option<u32>, size: (u16, u16), start: Start) -> io::Result<u32> {
        let Start { snapshot_in, banner, type_in, feed_in, proc_of } = start;
        let id = meta.id;
        let sock = self.paths.keeper_sock(id);
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock)?;
        let shell = self.gopt("default-shell");
        let (argv, arg0) = if meta.argv.is_empty() {
            let dc = self.gopt("default-command");
            if dc.is_empty() {
                let base = shell.rsplit('/').next().unwrap_or("sh").to_string();
                (vec![shell], Some(format!("-{}", base)))
            } else {
                (vec![shell, "-c".into(), dc], None)
            }
        } else if meta.argv.len() == 1 {
            (vec![shell, "-c".into(), meta.argv[0].clone()], None)
        } else {
            (meta.argv.clone(), None)
        };
        if meta.cwd.is_empty() || !std::path::Path::new(&meta.cwd).is_dir() {
            meta.cwd = util::home();
        }
        let spec = Spec {
            id,
            sock: sock.to_string_lossy().into_owned(),
            argv,
            arg0,
            cwd: meta.cwd.clone(),
            env: self.pane_env(session, id),
            cols: size.0.max(2),
            rows: size.1.max(1),
            history: self.gnum("history-limit", 2000).max(0) as usize,
            log: self.paths.log_file().to_string_lossy().into_owned(),
            snapshot_in,
            banner,
            type_in,
            feed_in,
            proc_of,
        };
        let json = serde_json::to_string(&spec).map_err(io::Error::other)?;
        let lfd: OwnedFd = listener.into();
        pty::spawn_daemon(&[exe_path(), "__keeper".into(), json], Some(&lfd), Some(&self.paths.log_file()))?;
        let s = UnixStream::connect(&sock)?;
        drop(lfd);
        let conn = Conn::new(s)?;
        if meta.created == 0 {
            meta.created = util::now_unix();
        }
        let mut p = Pane::new(meta, Some(conn), size, &self.agent_kinds);
        p.send_hello();
        let colors = self.clients.iter().find_map(|c| c.env.iter().find(|(k, _)| k == "__AMUX_COLORS").map(|(_, v)| v.clone()));
        if let Some(c) = colors {
            p.send(s2k::SETOPT, &Enc::new().str("colors").str(&c).buf);
        }
        self.panes.insert(id, p);
        self.next_pane = self.next_pane.max(id + 1);
        self.mark_dirty();
        Ok(id)
    }

    pub fn alloc_pane_id(&mut self) -> u32 {
        let mut id = self.next_pane;
        while self.panes.contains_key(&id) || self.paths.keeper_sock(id).exists() && UnixStream::connect(self.paths.keeper_sock(id)).is_ok() {
            id += 1;
        }
        self.next_pane = id + 1;
        id
    }

    /// Remove a pane from `self.panes` and ask its keeper to close.
    pub fn close_pane_keeper(&mut self, id: u32) {
        if let Some(mut p) = self.panes.remove(&id) {
            p.send(s2k::CLOSE, &[]);
            if let Some(c) = p.conn.as_mut() {
                let _ = c.flush_blocking();
            }
        }
    }

    fn remove_pane(&mut self, id: u32) {
        if self.panes.contains_key(&id) {
            self.emit_event("pane-closed", id, "");
            self.plugins_forget_pane(id);
        }
        self.close_pane_keeper(id);
        let _ = std::fs::remove_file(self.paths.snap_file(id));
    }

    /// Remove a pane from its window (and the window/session if empty).
    pub fn destroy_pane(&mut self, id: u32) {
        self.remove_pane(id);
        self.settle_pane_waits(id);
        let wid = self.windows.values().find(|w| w.layout.contains(id)).map(|w| w.id);
        if let Some(wid) = wid {
            let w = self.windows.get_mut(&wid).unwrap();
            if !w.layout.remove(id) {
                self.destroy_window(wid);
            } else {
                if w.active == id {
                    w.active = w.last.filter(|l| w.layout.contains(*l)).unwrap_or_else(|| w.layout.panes()[0]);
                }
                if w.last == Some(id) {
                    w.last = None;
                }
                w.zoomed = false;
                let (sx, sy) = (w.sx, w.sy);
                w.layout.fit(sx, sy);
                self.resize_window_panes(wid);
            }
        }
        self.mark_dirty();
        self.redraw_all();
    }

    pub fn destroy_window(&mut self, wid: u32) {
        if let Some(w) = self.windows.remove(&wid) {
            for p in w.layout.panes() {
                self.remove_pane(p);
            }
        }
        let renumber = self.gflag("renumber-windows");
        let base = self.gnum("base-index", 0).max(0) as u32;
        let mut empty = Vec::new();
        for s in &mut self.sessions {
            if s.windows.iter().any(|(_, w)| *w == wid) {
                s.windows.retain(|(_, w)| *w != wid);
                if s.active == wid {
                    s.active = s.last.filter(|l| s.windows.iter().any(|(_, w)| w == l)).or_else(|| s.windows.first().map(|(_, w)| *w)).unwrap_or(0);
                }
                if s.last == Some(wid) {
                    s.last = None;
                }
                if renumber {
                    for (i, e) in s.windows.iter_mut().enumerate() {
                        e.0 = base + i as u32;
                    }
                }
                if s.windows.is_empty() {
                    empty.push(s.id);
                }
            }
        }
        for sid in empty {
            self.destroy_session(sid);
        }
        self.mark_dirty();
    }

    pub fn destroy_session(&mut self, sid: u32) {
        let wins: Vec<u32> = self.sessions.iter().find(|s| s.id == sid).map(|s| s.windows.iter().map(|(_, w)| *w).collect()).unwrap_or_default();
        self.sessions.retain(|s| s.id != sid);
        for w in wins {
            self.destroy_window(w);
        }
        // Move or detach clients attached to it.
        let other = self.sessions.iter().max_by_key(|s| (s.activity, s.id)).map(|s| s.id);
        let attached: Vec<u32> = self.clients.iter().filter(|c| c.attached == Some(sid)).map(|c| c.id).collect();
        for cid in attached {
            match other {
                Some(o) => self.attach(cid, o),
                None => self.detach_client(cid, "[exited]"),
            }
        }
        if self.sessions.is_empty() && self.gflag("exit-empty") {
            logf!("no sessions left; exiting");
            self.quit = Some(0);
        }
        self.mark_dirty();
    }

    /// Resize every pane in a window to its layout rectangle.
    pub fn resize_window_panes(&mut self, wid: u32) {
        for (pid, r) in self.pane_rects(wid) {
            if let Some(p) = self.panes.get_mut(&pid) {
                let size = (r.w.max(2), r.h.max(1));
                if p.size != size {
                    p.size = size;
                    p.send(s2k::RESIZE, &Enc::new().u16(size.0).u16(size.1).buf);
                }
            }
        }
    }

    /// Window size follows the most recently active client on its session.
    pub fn update_window_sizes(&mut self) {
        let status_lines = |srv: &Server, sid: u32| -> u16 { if srv.opt(Some(sid), None, "status") == "off" { 0 } else { 1 } };
        let mut sizes: HashMap<u32, (u16, u16, Instant)> = HashMap::new();
        for c in &self.clients {
            if let Some(sid) = c.attached {
                let e = sizes.entry(sid).or_insert((c.cols, c.rows, c.activity));
                if c.activity >= e.2 {
                    *e = (c.cols, c.rows, c.activity);
                }
            }
        }
        let mut changed = Vec::new();
        for s in &self.sessions {
            let Some(&(cols, rows, _)) = sizes.get(&s.id) else { continue };
            let sl = status_lines(self, s.id);
            let (wx, wy) = (cols.max(2), rows.saturating_sub(sl).max(1));
            for (_, wid) in &s.windows {
                if let Some(w) = self.windows.get(wid)
                    && (w.sx, w.sy) != (wx, wy)
                {
                    changed.push((*wid, wx, wy));
                }
            }
        }
        for (wid, wx, wy) in changed {
            if let Some(w) = self.windows.get_mut(&wid) {
                w.sx = wx;
                w.sy = wy;
                w.layout.fit(wx, wy);
            }
            self.resize_window_panes(wid);
            self.mark_dirty();
        }
    }

    pub fn session_of_window(&self, wid: u32) -> Option<u32> {
        self.sessions.iter().find(|s| s.windows.iter().any(|(_, w)| *w == wid)).map(|s| s.id)
    }

    pub fn window_of_pane(&self, pid: u32) -> Option<u32> {
        self.windows.values().find(|w| w.layout.contains(pid)).map(|w| w.id)
    }

    pub fn pane_rects(&self, wid: u32) -> Vec<(u32, Rect)> {
        let Some(w) = self.windows.get(&wid) else { return vec![] };
        let mut v = Vec::new();
        if w.zoomed {
            v.push((w.active, Rect { x: 0, y: 0, w: w.sx, h: w.sy }));
        } else {
            w.layout.layout(Rect { x: 0, y: 0, w: w.sx, h: w.sy }, &mut v);
        }
        v
    }

    pub fn redraw_all(&mut self) {
        self.status_gen += 1;
        for c in &mut self.clients {
            if c.attached.is_some() {
                c.dirty = true;
            }
        }
    }

    pub fn redraw_session(&mut self, sid: u32) {
        self.status_gen += 1;
        for c in &mut self.clients {
            if c.attached == Some(sid) {
                c.dirty = true;
            }
        }
    }

    pub fn redraw_window(&mut self, wid: u32) {
        let sid = self.session_of_window(wid);
        for c in &mut self.clients {
            if c.attached.is_some() && (c.attached == sid || matches!(c.overlay, Some(Overlay::Tree { .. } | Overlay::Agents { .. }))) {
                c.dirty = true;
            }
        }
    }

    /// Attached client to act on: `cid` if attached, else the most recently
    /// active attached client (preferring `prefer_session` when given).
    pub fn target_attached_client(&self, cid: u32, prefer_session: Option<u32>) -> Option<u32> {
        let usable = |c: &Client| c.attached.is_some() && !c.closing;
        if self.client_idx(cid).is_some_and(|i| usable(&self.clients[i])) {
            return Some(cid);
        }
        self.clients.iter().filter(|c| usable(c)).max_by_key(|c| (prefer_session.is_some() && c.attached == prefer_session, c.activity)).map(|c| c.id)
    }

    pub fn client_idx(&self, cid: u32) -> Option<usize> {
        self.clients.iter().position(|c| c.id == cid)
    }

    pub fn detach_client(&mut self, cid: u32, msg: &str) {
        if let Some(sid) = self.client_idx(cid).and_then(|i| self.clients[i].attached) {
            self.client_event("client-detached", cid, Some(sid));
        }
        if let Some(i) = self.client_idx(cid) {
            let c = &mut self.clients[i];
            if c.attached.is_some() {
                let reset = render::terminal_reset();
                c.out(s2c::OUTPUT, &reset);
            }
            c.attached = None;
            let mut e = Enc::new();
            e.i32(0).str(msg);
            c.out(s2c::EXIT, &e.buf);
            let _ = c.conn.flush();
            c.closing = true;
        }
    }

    // ----- main loop ---------------------------------------------------

    fn run(&mut self, sigs: &util::Signals) -> i32 {
        loop {
            if let Some(code) = self.quit {
                // A client starting now must not connect to a server that is
                // going away (stopping plugins can take a moment).
                let _ = std::fs::remove_file(self.paths.server_sock());
                self.plugins_shutdown();
                for c in &mut self.clients {
                    let _ = c.conn.flush_blocking();
                }
                if self.state_dirty {
                    self.save_state();
                }
                return code;
            }
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.iteration(sigs)));
            if r.is_err() {
                // Crash-only: let the supervisor restart us from persisted state.
                logf!("server panic; exiting for restart");
                return 70;
            }
        }
    }

    fn iteration(&mut self, sigs: &util::Signals) {
        let mut fds = vec![util::pfd(sigs.fd(), util::POLLIN), util::pfd(self.listener.as_raw_fd(), util::POLLIN)];
        let client_base = fds.len();
        for c in &self.clients {
            let mut e = util::POLLIN;
            if c.conn.pending_out() > 0 {
                e |= util::POLLOUT;
            }
            fds.push(util::pfd(c.conn.fd(), e));
        }
        let pane_base = fds.len();
        let pane_ids: Vec<u32> = self.panes.keys().copied().collect();
        for id in &pane_ids {
            let p = &self.panes[id];
            match &p.conn {
                Some(c) => {
                    let mut e = util::POLLIN;
                    if c.pending_out() > 0 {
                        e |= util::POLLOUT;
                    }
                    fds.push(util::pfd(c.fd(), e));
                }
                None => fds.push(util::pfd(-1, 0)),
            }
        }
        let job_base = fds.len();
        for j in &self.jobs {
            fds.push(util::pfd(if j.eof { -1 } else { j.out.as_raw_fd() }, util::POLLIN));
        }
        let plugin_base = fds.len();
        let mut plugin_fds = Vec::new();
        self.plugins_poll_fds(&mut plugin_fds);
        for (_, _, fd, ev) in &plugin_fds {
            fds.push(util::pfd(*fd, *ev));
        }
        let timeout = util::timeout_until(self.next_deadline());
        let _ = util::poll(&mut fds, timeout);

        if fds[0].revents != 0 {
            for s in sigs.drain() {
                match s {
                    libc::SIGCHLD => self.reap_children(),
                    libc::SIGTERM => {
                        logf!("SIGTERM: saving state and exiting (panes keep running)");
                        self.save_state();
                        // Non-zero so a supervisor restarts us unless it is also stopping.
                        self.quit = Some(75);
                    }
                    libc::SIGUSR1 => {
                        let _ = std::fs::remove_file(self.paths.server_sock());
                        if let Ok(l) = UnixListener::bind(self.paths.server_sock()) {
                            let _ = l.set_nonblocking(true);
                            self.listener = l;
                        }
                    }
                    _ => {}
                }
            }
        }
        if fds[1].revents != 0 {
            self.accept_clients();
        }
        let client_ids: Vec<u32> = self.clients.iter().map(|c| c.id).collect();
        for (k, cid) in client_ids.iter().enumerate() {
            let r = fds.get(client_base + k).map(|f| f.revents).unwrap_or(0);
            if r & (util::POLLIN | util::POLLERR) != 0 {
                self.read_client(*cid);
            }
        }
        for (k, id) in pane_ids.iter().enumerate() {
            let r = fds.get(pane_base + k).map(|f| f.revents).unwrap_or(0);
            if r & (util::POLLIN | util::POLLERR) != 0 {
                self.read_keeper(*id);
            }
        }
        let job_ids: Vec<u32> = self.jobs.iter().map(|j| j.id).collect();
        for (k, jid) in job_ids.iter().enumerate() {
            let r = fds.get(job_base + k).map(|f| f.revents).unwrap_or(0);
            if r != 0 {
                self.read_job(*jid);
            }
        }
        for (k, (id, kind, _, _)) in plugin_fds.iter().enumerate() {
            let r = fds.get(plugin_base + k).map(|f| f.revents).unwrap_or(0);
            if r != 0 {
                self.plugins_on_ready(*id, *kind);
            }
        }
        self.timers();
        self.plugins_pump();
        self.plugins_check_waits();
        if self.state_dirty {
            self.save_state();
        }
        self.render_clients();
        self.flush_all();
        self.reap_closed_clients();
    }

    fn next_deadline(&self) -> Option<Instant> {
        let now = Instant::now();
        let mut d: Option<Instant> = Some(now + Duration::from_secs(2));
        for c in &self.clients {
            d = util::earliest(d, c.esc_deadline);
            d = util::earliest(d, c.repeat_until);
            d = util::earliest(d, c.message.as_ref().map(|m| m.1));
            if let Some(Overlay::Panes { until }) = &c.overlay {
                d = util::earliest(d, Some(*until));
            }
            if c.dirty && c.attached.is_some() {
                d = util::earliest(d, Some(c.last_render + MIN_RENDER_INTERVAL));
            }
            match &c.waiting {
                Some(Wait::Exit { deadline, .. } | Wait::Pattern { deadline, .. } | Wait::Agent { deadline, .. }) => {
                    d = util::earliest(d, *deadline);
                }
                Some(
                    Wait::Capture { deadline, .. } | Wait::Plugin { deadline, .. } | Wait::PluginLoad { deadline, .. } | Wait::PluginSettle { deadline, .. },
                ) => d = util::earliest(d, Some(*deadline)),
                Some(Wait::Idle { .. }) => d = util::earliest(d, Some(now + Duration::from_millis(100))),
                _ => {}
            }
        }
        if let Some(t) = self.meta_dirty_at {
            d = util::earliest(d, Some(t));
        }
        let d = util::earliest(d, self.plugins_view_deadline());
        util::earliest(d, self.plugins.deadline())
    }

    fn timers(&mut self) {
        let _t = util::prof::time(util::prof::TIMERS);
        let now = Instant::now();
        // Heartbeat for the supervisor's hang watchdog.
        if let Some(h) = &self.heartbeat
            && now.duration_since(self.last_heartbeat) >= Duration::from_secs(2)
        {
            let _ = rustix::io::write(h, b".");
            self.last_heartbeat = now;
        }
        for i in 0..self.clients.len() {
            if let Some(dl) = self.clients[i].esc_deadline
                && now >= dl
            {
                self.clients[i].esc_deadline = None;
                let evs = self.clients[i].parser.timeout();
                let cid = self.clients[i].id;
                for e in evs {
                    self.handle_input_event(cid, e);
                }
            }
            let Some(c) = self.clients.get_mut(i) else { continue };
            if let Some(r) = c.repeat_until
                && now >= r
            {
                c.repeat_until = None;
                c.key_table = "root".into();
                c.dirty = true;
            }
            if let Some((_, until)) = &c.message
                && now >= *until
            {
                c.message = None;
                c.dirty = true;
            }
            if let Some(Overlay::Panes { until }) = &c.overlay
                && now >= *until
            {
                c.overlay = None;
                c.dirty = true;
            }
        }
        // Status line refresh (clock, #() commands).
        let interval = self.gnum("status-interval", 15).max(1) as u64;
        if now.duration_since(self.last_status_tick) >= Duration::from_secs(interval) {
            self.last_status_tick = now;
            self.redraw_all();
        }
        if now.duration_since(self.last_agent_tick) >= Duration::from_millis(1000) {
            self.last_agent_tick = now;
            self.agent_tick();
        }
        // Periodic scrollback snapshots for reboot resurrection.
        let snap_iv = self.gnum("snapshot-interval", 30);
        if snap_iv > 0 && now.duration_since(self.last_snapshot) >= Duration::from_secs(snap_iv as u64) {
            self.last_snapshot = now;
            self.snapshot_panes(false);
        }
        if let Some(t) = self.meta_dirty_at
            && now >= t
        {
            self.meta_dirty_at = None;
            self.state_dirty = true;
        }
        self.check_waits();
        self.plugins_timers();
        self.plugins_view_poll();
    }

    pub fn snapshot_panes(&mut self, all: bool) {
        let ids: Vec<u32> = self.panes.keys().copied().collect();
        for id in ids {
            let path = self.paths.snap_file(id).to_string_lossy().into_owned();
            let p = self.panes.get_mut(&id).unwrap();
            if all || p.dirty_since_snapshot {
                p.dirty_since_snapshot = false;
                p.send(s2k::SNAPSHOT, &Enc::new().str(&path).buf);
            }
        }
    }

    fn reap_children(&mut self) {
        for (pid, st) in pty::reap() {
            if let Some(j) = self.jobs.iter_mut().find(|j| j.pid == pid) {
                j.done = Some(pty::exit_code(st));
            } else {
                self.plugins_reaped(pid, st);
            }
        }
        let ids: Vec<u32> = self.jobs.iter().filter(|j| j.done.is_some() && j.eof).map(|j| j.id).collect();
        for id in ids {
            self.finish_job(id);
        }
    }

    fn accept_clients(&mut self) {
        while let Ok((s, _)) = self.listener.accept() {
            if let Ok(conn) = Conn::new(s) {
                let id = self.next_client;
                self.next_client += 1;
                self.clients.push(Client::new(id, conn));
            }
        }
    }

    fn reap_closed_clients(&mut self) {
        let lost: Vec<(u32, u32)> = self.clients.iter().filter(|c| c.conn.eof && !c.closing).filter_map(|c| c.attached.map(|s| (c.id, s))).collect();
        for (cid, sid) in lost {
            self.client_event("client-detached", cid, Some(sid));
        }
        let before = self.clients.len();
        self.clients.retain(|c| !(c.conn.eof || (c.closing && c.conn.pending_out() == 0)));
        if self.clients.len() != before {
            self.update_window_sizes();
            if self.gflag("exit-unattached") && !self.clients.iter().any(|c| c.attached.is_some()) && !self.sessions.is_empty() {
                self.quit = Some(0);
            }
        }
    }

    fn flush_all(&mut self) {
        let _t = util::prof::time(util::prof::FLUSH);
        for c in &mut self.clients {
            if c.conn.flush().is_err() {
                c.conn.eof = true;
            }
        }
        let mut lost = Vec::new();
        for (id, p) in self.panes.iter_mut() {
            if let Some(c) = p.conn.as_mut()
                && c.flush().is_err()
            {
                lost.push(*id);
            }
        }
        for id in lost {
            self.keeper_lost(id);
        }
    }

    fn read_client(&mut self, cid: u32) {
        let _t = util::prof::time(util::prof::CLIENT);
        let Some(i) = self.client_idx(cid) else { return };
        let _ = self.clients[i].conn.fill();
        let mut msgs = Vec::new();
        loop {
            match self.clients[i].conn.next() {
                Ok(Some(m)) => msgs.push(m),
                Ok(None) => break,
                Err(_) => {
                    self.clients[i].conn.eof = true;
                    break;
                }
            }
        }
        for (ty, p) in msgs {
            if let Err(e) = self.handle_client_msg(cid, ty, &p) {
                logf!("client {} bad message {}: {}", cid, ty, e);
            }
        }
    }

    fn handle_client_msg(&mut self, cid: u32, ty: u8, p: &[u8]) -> Result<(), proto::Bad> {
        let mut d = Dec::new(p);
        match ty {
            c2s::HELLO => {
                let _ver = d.u32()?;
                let _flags = d.u32()?;
                let cols = d.u16()?;
                let rows = d.u16()?;
                let term = d.string()?;
                let colorterm = d.string()?;
                let cwd = d.string()?;
                let pid = d.u32()?;
                let n = d.len()?;
                let mut env = Vec::new();
                for _ in 0..n {
                    let k = d.string()?;
                    let v = d.string()?;
                    env.push((k, v));
                }
                let same_server =
                    env.iter().find(|(k, _)| k == "AMUX").and_then(|(_, v)| v.split(',').next()).is_none_or(|s| {
                        std::path::Path::new(s) == self.paths.server_sock() || util::same_file(std::path::Path::new(s), &self.paths.server_sock())
                    });
                let Some(i) = self.client_idx(cid) else { return Ok(()) };
                let c = &mut self.clients[i];
                c.cols = cols.max(2);
                c.rows = rows.max(2);
                c.term = term;
                c.truecolor = colorterm == "truecolor"
                    || colorterm == "24bit"
                    || matches!(c.term.as_str(), "xterm-kitty" | "xterm-ghostty" | "wezterm" | "alacritty" | "foot" | "foot-extra");
                c.cwd = cwd;
                c.pid = pid;
                c.caller_pane =
                    if same_server { env.iter().find(|(k, _)| k == "AMUX_PANE").and_then(|(_, v)| v.trim_start_matches('%').parse().ok()) } else { None };
                c.env = env;
            }
            c2s::CMD => {
                let argv = d.strs()?;
                let Some(i) = self.client_idx(cid) else { return Ok(()) };
                self.clients[i].cli = true;
                let cmds = crate::cmd::split_semicolons(&argv);
                let cmds = if cmds.is_empty() { vec![vec!["new-session".to_string()]] } else { cmds };
                for c in cmds {
                    self.clients[i].queue.push_back(c);
                }
                self.run_queue(cid);
            }
            c2s::INPUT => {
                let data = d.rest().to_vec();
                self.client_input(cid, &data);
            }
            c2s::RESIZE => {
                let cols = d.u16()?;
                let rows = d.u16()?;
                if let Some(i) = self.client_idx(cid) {
                    let c = &mut self.clients[i];
                    c.cols = cols.max(2);
                    c.rows = rows.max(2);
                    c.full_redraw = true;
                    c.dirty = true;
                    c.activity = Instant::now();
                }
                self.update_window_sizes();
            }
            c2s::DETACH => self.detach_client(cid, "[detached]"),
            c2s::COLORS => {
                let colors = d.string()?;
                if let Some(i) = self.client_idx(cid) {
                    self.clients[i].env.push(("__AMUX_COLORS".into(), colors.clone()));
                }
                for p in self.panes.values_mut() {
                    p.send(s2k::SETOPT, &Enc::new().str("colors").str(&colors).buf);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn keeper_lost(&mut self, id: u32) {
        let Some(p) = self.panes.get_mut(&id) else { return };
        if p.conn.is_none() {
            return;
        }
        p.conn = None;
        logf!("lost keeper for pane {}", id);
        // The keeper died (killed?). Its program is gone with it; resurrect.
        let meta = p.meta.clone();
        let size = p.size;
        self.panes.remove(&id);
        let session = self.window_of_pane(id).and_then(|w| self.session_of_window(w));
        let snap = self.paths.snap_file(id);
        let snap_in = snap.exists().then(|| snap.to_string_lossy().into_owned());
        let banner = Some("\x1b[2m── amux: pane process was lost; respawned ──\x1b[0m\r\n".to_string());
        if let Err(e) = self.spawn_keeper(PaneMeta { argv: vec![], ..meta }, session, size, snap_in, banner, None) {
            logf!("respawn of pane {} failed: {}", id, e);
            self.destroy_pane(id);
        } else {
            self.settle_pane_waits(id);
        }
        self.redraw_all();
    }

    fn read_keeper(&mut self, id: u32) {
        let _t = util::prof::time(util::prof::KEEPER);
        let Some(p) = self.panes.get_mut(&id) else { return };
        let Some(c) = p.conn.as_mut() else { return };
        let _ = c.fill();
        let mut msgs = Vec::new();
        loop {
            match c.next() {
                Ok(Some(m)) => msgs.push(m),
                Ok(None) => break,
                Err(_) => {
                    c.eof = true;
                    break;
                }
            }
        }
        let eof = c.eof;
        for (ty, payload) in msgs {
            if let Err(e) = self.handle_keeper_msg(id, ty, &payload) {
                logf!("pane {} bad message {}: {}", id, ty, e);
            }
        }
        if eof {
            let exited = self.panes.get(&id).map(|p| p.exited.is_some()).unwrap_or(true);
            if exited {
                // Normal after CLOSE.
                if let Some(p) = self.panes.get_mut(&id) {
                    p.conn = None;
                }
            } else {
                self.keeper_lost(id);
            }
        }
    }

    fn handle_keeper_msg(&mut self, id: u32, ty: u8, payload: &[u8]) -> Result<(), proto::Bad> {
        let mut d = Dec::new(payload);
        match ty {
            k2s::INFO => {
                let _pid = d.u32()?;
                let kpid = d.u32()?;
                let child = d.u32()?;
                let _cols = d.u16()?;
                let _rows = d.u16()?;
                let dead = d.u8()? != 0;
                let status = d.i32()?;
                let title = d.string()?;
                let cwd = d.string()?;
                let argv = d.strs()?;
                let started = d.u64()?;
                let recoveries = d.u32()?;
                if let Some(p) = self.panes.get_mut(&id) {
                    p.keeper_pid = kpid;
                    p.child_pid = child;
                    p.ready = true;
                    p.recoveries = recoveries;
                    if p.meta.cwd.is_empty() {
                        p.meta.cwd = cwd;
                    }
                    if p.meta.argv.is_empty() && argv.len() > 1 && argv.get(1).map(|s| s.as_str()) == Some("-c") {
                        p.meta.argv = vec![argv[2..].join(" ")];
                    }
                    p.meta.title = title;
                    if p.meta.created == 0 {
                        p.meta.created = started;
                    }
                    if dead {
                        p.exited = Some(status);
                    }
                    // Make sure the keeper has the layout size.
                    p.size = (0, 0);
                }
                if let Some(w) = self.window_of_pane(id) {
                    self.resize_window_panes(w);
                }
            }
            k2s::FRAME => {
                let f = keeper::decode_frame(payload)?;
                let now = Instant::now();
                let epoch = util::now_unix();
                let mut copy_changed = false;
                if let Some(p) = self.panes.get_mut(&id) {
                    if p.in_copy != f.copy {
                        copy_changed = true;
                        p.in_copy = f.copy;
                    }
                    if !f.copy {
                        p.agent.note_output(now, payload.len(), epoch);
                        p.dirty_since_snapshot = true;
                    }
                    let title_changed = f.title != p.mirror.title;
                    p.mirror.apply(f);
                    p.send(s2k::ACK, &[]);
                    if title_changed || copy_changed {
                        self.status_gen += 1;
                    }
                }
                if let Some(w) = self.window_of_pane(id) {
                    if let Some(win) = self.windows.get_mut(&w)
                        && win.active != id
                        && !copy_changed
                        && !win.activity
                    {
                        win.activity = true;
                        self.status_gen += 1;
                    }
                    self.redraw_window(w);
                }
                self.check_pattern_waits(id);
                self.plugins_note_output(id);
            }
            k2s::EVENT => {
                let kind = d.u8()?;
                self.keeper_event(id, kind, &mut d)?;
            }
            k2s::EXITED => {
                let st = d.i32()?;
                self.pane_exited(id, st);
            }
            k2s::CAPTURED => {
                let req = d.u32()?;
                let text = d.string()?;
                self.capture_reply(req, text);
            }
            k2s::COPIED => {
                let text = d.string()?;
                let pipe = d.string()?;
                self.add_buffer(None, text.clone());
                self.set_clipboard(&text);
                if !pipe.is_empty() {
                    self.start_job(&pipe, None, Some(text.into_bytes()), false, vec![], vec![]);
                }
            }
            k2s::PROC => {
                let pg = d.u32()?;
                let cmd = d.string()?;
                let cwd = d.string()?;
                let kind = self.agent_kinds.detect(&cmd);
                let mut changed = None;
                if let Some(p) = self.panes.get_mut(&id) {
                    p.fg_pid = pg;
                    let mut persist = false;
                    if !cwd.is_empty() && cwd != p.meta.cwd {
                        p.meta.cwd = cwd;
                        persist = true;
                    }
                    p.meta.fg_cmd = cmd;
                    // A pane marked with `status set` stays marked while the
                    // program that set the mark is in the foreground (marks set
                    // from elsewhere stay until cleared). A newly recognised
                    // agent keeps the mark: its hooks most likely set it.
                    let marked = p.agent.agent.as_ref().is_some_and(|k| k.is_generic());
                    let owner_left = p.agent.mark_pg.is_some_and(|m| m != pg);
                    let next = if marked && kind.is_none() && !owner_left { p.agent.agent.clone() } else { kind };
                    if !agents::same(&next, &p.agent.agent) {
                        // Persisted so a resurrected pane can resume this agent.
                        p.meta.agent = next.as_ref().map(|k| k.def.name.clone());
                        p.meta.agent_resume = next.as_ref().and_then(|k| k.def.resume.clone());
                        if !(marked && next.is_some()) {
                            p.agent.explicit = None;
                        }
                        p.agent.agent = next;
                        persist = true;
                    }
                    changed = Some(persist);
                }
                if let Some(persist) = changed {
                    self.status_gen += 1;
                    // Only what resurrection needs is saved (not every command).
                    if persist {
                        self.meta_dirty_at.get_or_insert(Instant::now() + Duration::from_secs(2));
                    }
                    if let Some(w) = self.window_of_pane(id) {
                        self.redraw_window(w);
                    }
                }
            }
            k2s::MODE => {
                let copy = d.u8()? != 0;
                if let Some(p) = self.panes.get_mut(&id) {
                    p.in_copy = copy;
                }
                if let Some(w) = self.window_of_pane(id) {
                    self.redraw_window(w);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn keeper_event(&mut self, id: u32, kind: u8, d: &mut Dec) -> Result<(), proto::Bad> {
        let now = Instant::now();
        self.status_gen += 1;
        match kind {
            ev::BELL => {
                if let Some(p) = self.panes.get_mut(&id) {
                    p.agent.bell = true;
                }
                if let Some(w) = self.window_of_pane(id) {
                    let active = self.windows.get(&w).map(|win| win.active == id).unwrap_or(false);
                    if let Some(win) = self.windows.get_mut(&w)
                        && !active
                    {
                        win.bell = true;
                    }
                    if self.gopt("bell-action") != "none" {
                        let sid = self.session_of_window(w);
                        let any = self.gopt("bell-action") == "any";
                        for c in &mut self.clients {
                            if c.attached.is_some() && (any || c.attached == sid) {
                                c.out(s2c::OUTPUT, b"\x07");
                            }
                        }
                    }
                    self.redraw_window(w);
                }
                self.emit_event("bell", id, "");
            }
            ev::TITLE => {
                let t = d.string()?;
                if let Some(p) = self.panes.get_mut(&id) {
                    p.meta.title = t;
                }
                if let Some(w) = self.window_of_pane(id) {
                    self.redraw_window(w);
                }
            }
            ev::CWD => {
                let c = d.string()?;
                if let Some(p) = self.panes.get_mut(&id) {
                    p.meta.cwd = c;
                }
                self.meta_dirty_at.get_or_insert(now + Duration::from_secs(2));
            }
            ev::NOTIFY => {
                let t = d.string()?;
                let b = d.string()?;
                let msg = if t.is_empty() { b } else { format!("{}: {}", t, b) };
                if let Some(p) = self.panes.get_mut(&id) {
                    p.agent.notified = Some((msg.clone(), now));
                    p.agent.bell = true;
                }
                self.notify_clients(&msg);
                let mut extra = serde_json::Map::new();
                extra.insert("message".into(), msg.clone().into());
                self.emit_event_with("notify", id, &msg, extra);
                self.redraw_all();
            }
            ev::PROGRESS => {
                let s = d.u8()?;
                let v = d.u8()?;
                if let Some(p) = self.panes.get_mut(&id) {
                    p.agent.progress = Some((s, v, now));
                }
            }
            ev::CLIPBOARD => {
                let b64 = d.string()?;
                if self.gopt("set-clipboard") != "off"
                    && let Some(bytes) = util::base64_decode(&b64)
                {
                    let text = String::from_utf8_lossy(&bytes).into_owned();
                    self.add_buffer(None, text.clone());
                    self.set_clipboard(&text);
                }
            }
            ev::PROMPT => {
                let k = d.u8()?;
                let has = d.u8()? != 0;
                let code = d.i32()?;
                if k == b'D' && has {
                    let mut extra = serde_json::Map::new();
                    extra.insert("code".into(), code.into());
                    self.emit_event_with("command-done", id, &code.to_string(), extra);
                }
            }
            ev::RECOVERED => {
                let n = d.u32()?;
                if let Some(p) = self.panes.get_mut(&id) {
                    p.recoveries = n;
                }
                self.status_message_all(&format!("pane %{} emulator recovered from an internal error", id));
            }
            _ => {}
        }
        Ok(())
    }

    fn pane_exited(&mut self, id: u32, status: i32) {
        let Some(p) = self.panes.get_mut(&id) else { return };
        p.exited = Some(status);
        let code = pty::exit_code(status);
        let mut extra = serde_json::Map::new();
        extra.insert("code".into(), code.into());
        self.emit_event_with("exit", id, &code.to_string(), extra);
        // Waiters may still need the pane (e.g. `amux exec` captures it).
        let held = self.clients.iter().any(|c| matches!(&c.waiting, Some(Wait::Exit { pane, .. }) if *pane == id));
        self.check_exit_waits(id);
        let wid = self.window_of_pane(id);
        let remain = wid.map(|w| self.opt(self.session_of_window(w), Some(w), "remain-on-exit")).unwrap_or_default();
        if remain == "on" || held || (remain == "failed" && code != 0) {
            let msg = format!("\r\n\x1b[0m\x1b[7mPane is dead (status {}, {})\x1b[0m", code, format::strftime("%a %b %e %H:%M:%S %Y"));
            if let Some(p) = self.panes.get_mut(&id) {
                p.send(s2k::ECHO, msg.as_bytes());
            }
            if let Some(w) = wid {
                self.redraw_window(w);
            }
        } else {
            self.destroy_pane(id);
        }
    }

    pub fn status_message_all(&mut self, msg: &str) {
        let until = Instant::now() + Duration::from_millis(self.gnum("display-time", 750).max(1500) as u64);
        for c in &mut self.clients {
            if c.attached.is_some() {
                c.message = Some((msg.to_string(), until));
                c.dirty = true;
            }
        }
        self.log_message(msg);
    }

    pub fn log_message(&mut self, msg: &str) {
        self.messages.push_back(format!("{} {}", format::strftime("%H:%M:%S"), msg));
        while self.messages.len() > 200 {
            self.messages.pop_front();
        }
    }

    pub fn notify_clients(&mut self, msg: &str) {
        let clean: String = msg.chars().filter(|c| !c.is_control()).take(200).collect();
        let osc = format!("\x1b]9;{}\x07", clean);
        for c in &mut self.clients {
            if c.attached.is_some() {
                c.out(s2c::OUTPUT, osc.as_bytes());
            }
        }
    }

    pub fn set_clipboard(&mut self, text: &str) {
        if self.gopt("set-clipboard") == "off" {
            return;
        }
        let osc = format!("\x1b]52;c;{}\x07", util::base64_encode(text.as_bytes()));
        for c in &mut self.clients {
            if c.attached.is_some() {
                c.out(s2c::OUTPUT, osc.as_bytes());
            }
        }
    }

    pub fn add_buffer(&mut self, name: Option<String>, text: String) {
        let name = name.unwrap_or_else(|| {
            let n = format!("buffer{}", self.next_buffer);
            self.next_buffer += 1;
            n
        });
        self.buffers.retain(|(n, _)| n != &name);
        self.buffers.insert(0, (name, text));
        self.buffers.truncate(50);
    }

    /// Stream a JSON event to `amux events` subscribers and plugins.
    pub fn emit_event(&mut self, kind: &str, pane: u32, detail: &str) {
        self.emit_event_with(kind, pane, detail, serde_json::Map::new());
    }

    /// `emit_event` with event-specific fields for plugins.
    pub fn emit_event_with(&mut self, kind: &str, pane: u32, detail: &str, mut extra: serde_json::Map<String, serde_json::Value>) {
        let line = serde_json::json!({
            "time": util::now_ms(),
            "event": kind,
            "pane": format!("%{}", pane),
            "detail": detail,
        })
        .to_string();
        for c in &mut self.clients {
            if c.events_json {
                c.print(&line);
            }
        }
        if !detail.is_empty() {
            extra.entry("detail").or_insert_with(|| detail.into());
        }
        self.plugins_event(kind, Some(pane), extra);
    }

    /// A client attached or left: tell plugins.
    pub fn client_event(&mut self, kind: &str, cid: u32, sid: Option<u32>) {
        let mut extra = serde_json::Map::new();
        extra.insert("client".into(), format!("client{}", cid).into());
        if let Some(s) = sid.and_then(|s| self.sessions.iter().find(|x| x.id == s)) {
            extra.insert("session".into(), s.name.clone().into());
        }
        self.plugins_event(kind, None, extra);
    }

    pub fn visible_panes(&self) -> std::collections::HashSet<u32> {
        self.clients
            .iter()
            .filter_map(|c| c.attached)
            .filter_map(|sid| self.sessions.iter().find(|s| s.id == sid))
            .filter_map(|s| self.windows.get(&s.active))
            .map(|w| w.active)
            .collect()
    }

    /// An agent changed state: event, plus a notification when it needs you
    /// and you cannot see it.
    pub fn agent_changed(&mut self, id: u32, prev: AState, st: AState, summary: &str, visible: &std::collections::HashSet<u32>) {
        let name = self.panes.get(&id).and_then(|p| p.agent.agent.as_ref()).map(|k| k.def.name.clone()).unwrap_or_else(|| agents::GENERIC.into());
        let mut extra = serde_json::Map::new();
        extra.insert("prev".into(), prev.label().to_lowercase().into());
        self.emit_event_with("agent-state", id, &format!("{}:{}", name, st.label().to_lowercase()), extra);
        let attention = matches!(st, AState::Waiting) || (st == AState::Done && prev == AState::Working);
        if attention && !visible.contains(&id) && self.gflag("agent-badge") {
            let w = self.window_of_pane(id).and_then(|w| self.windows.get(&w)).map(|w| w.name.clone()).unwrap_or_default();
            self.notify_clients(&format!("{} in {} {}: {}", name, w, st.label().to_lowercase(), summary));
        }
    }

    // ----- agents ------------------------------------------------------

    /// Agent definitions changed (a plugin added, replaced or removed some):
    /// recognise panes again. Explicit statuses survive.
    pub fn agents_redetect(&mut self) {
        let mut changed = false;
        for p in self.panes.values_mut() {
            let cur = p.agent.agent.clone();
            let next = match self.agent_kinds.detect(&p.meta.fg_cmd) {
                Some(k) => Some(k),
                None => match &cur {
                    Some(k) if self.agent_kinds.current(k) => cur.clone(),
                    Some(_) if p.agent.explicit.is_some() => Some(self.agent_kinds.generic()),
                    _ => None,
                },
            };
            if agents::same(&next, &cur) {
                continue;
            }
            if let Some(k) = &next {
                p.meta.agent = Some(k.def.name.clone());
                p.meta.agent_resume = k.def.resume.clone();
            }
            p.agent.agent = next;
            changed = true;
        }
        if changed {
            self.status_gen += 1;
            self.meta_dirty_at.get_or_insert(Instant::now() + Duration::from_secs(2));
            self.redraw_all();
        }
    }

    fn agent_tick(&mut self) {
        let _t = util::prof::time(util::prof::AGENTS);
        let now = Instant::now();
        let epoch = util::now_unix();
        let visible = self.visible_panes();
        let mut changes = Vec::new();
        for (id, p) in self.panes.iter_mut() {
            if p.agent.agent.is_none() {
                continue;
            }
            let lines = p.mirror.lines();
            let prev = p.agent.state;
            if p.agent.update(now, &lines, &p.meta.title, p.exited.is_some(), epoch) {
                changes.push((*id, prev, p.agent.state, p.agent.summary.clone()));
            }
            if visible.contains(id) && p.agent.state == AState::Done {
                p.agent.seen();
            }
        }
        if changes.is_empty() {
            return;
        }
        for (id, prev, st, summary) in changes {
            self.agent_changed(id, prev, st, &summary, &visible);
        }
        self.redraw_all();
        self.check_agent_waits();
    }

    // ----- rendering ---------------------------------------------------

    fn render_clients(&mut self) {
        let now = Instant::now();
        for i in 0..self.clients.len() {
            let c = &self.clients[i];
            if c.attached.is_none() || !c.dirty || c.closing {
                continue;
            }
            if now < c.last_render + MIN_RENDER_INTERVAL || c.conn.pending_out() > 256 * 1024 {
                continue;
            }
            let out = render::render(self, i);
            let c = &mut self.clients[i];
            c.dirty = false;
            c.last_render = now;
            if !out.is_empty() {
                c.out(s2c::OUTPUT, &out);
            }
        }
    }
}

impl Pane {
    pub fn new(meta: PaneMeta, conn: Option<Conn>, size: (u16, u16), kinds: &agents::Kinds) -> Pane {
        let agent = Tracker { agent: meta.agent.as_deref().and_then(|a| kinds.by_name(a)), ..Default::default() };
        Pane {
            meta,
            conn,
            mirror: Mirror::default(),
            exited: None,
            in_copy: false,
            keeper_pid: 0,
            child_pid: 0,
            fg_pid: 0,
            agent,
            dirty_since_snapshot: true,
            recoveries: 0,
            size,
            ready: false,
        }
    }

    pub fn send(&mut self, ty: u8, payload: &[u8]) {
        if let Some(c) = self.conn.as_mut() {
            c.send(ty, payload);
        }
    }

    pub fn send_hello(&mut self) {
        self.send(s2k::HELLO, &Enc::new().u32(proto::VERSION).buf);
    }

    pub fn input(&mut self, data: &[u8]) {
        if !data.is_empty() {
            self.send(s2k::INPUT, data);
            if self.agent.state == AState::Waiting {
                let lines = self.mirror.lines();
                self.agent.note_input(Instant::now(), &lines);
            }
        }
    }

    pub fn current_command(&self) -> String {
        let cmd = &self.meta.fg_cmd;
        let first = cmd.split_whitespace().next().unwrap_or("");
        let base = first.rsplit('/').next().unwrap_or(first).trim_start_matches('-');
        if let Some(k) = &self.agent.agent {
            return k.def.name.clone();
        }
        if base.is_empty() {
            return self.meta.argv.first().map(|s| s.rsplit('/').next().unwrap_or(s).to_string()).unwrap_or_else(|| "shell".into());
        }
        base.to_string()
    }
}

impl Client {
    fn new(id: u32, conn: Conn) -> Client {
        Client {
            id,
            conn,
            cols: 80,
            rows: 24,
            term: String::new(),
            truecolor: false,
            env: Vec::new(),
            cwd: String::new(),
            pid: 0,
            attached: None,
            parser: keys::Parser::default(),
            key_table: "root".into(),
            repeat_until: None,
            esc_deadline: None,
            message: None,
            prompt: None,
            overlay: None,
            front: Vec::new(),
            front_size: (0, 0),
            status_cache: None,
            spare: Vec::new(),
            clusters: Clusters::default(),
            cursor: (0, 0, false, 0),
            full_redraw: true,
            dirty: true,
            last_render: Instant::now() - Duration::from_secs(1),
            queue: VecDeque::new(),
            waiting: None,
            cli: false,
            exit_code: 0,
            drag: None,
            last_session: None,
            outer_mouse: false,
            outer_focus: false,
            activity: Instant::now(),
            caller_pane: None,
            closing: false,
            title: String::new(),
            events_json: false,
            plugin: None,
            sink: None,
            settle_retry: false,
        }
    }
}
