//! Pane keeper: one small process per pane that owns the PTY master, runs the
//! terminal emulator, and serves frames to whichever server is connected.
//!
//! The keeper is deliberately independent of the server: if the server
//! crashes or is restarted, the keeper (and the program in the pane) keeps
//! running, keeps answering terminal queries, and hands a full screen to the
//! next server that connects. Emulator panics are caught and recovered.

use crate::cell::*;
use crate::copymode::{CopyMode, Outcome};
use crate::proto::{self, Conn, Dec, Enc, ev, k2s, s2k};
use crate::term::{Emulator, Event};
use crate::{logf, pty, util};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Spec {
    pub id: u32,
    pub sock: String,
    pub argv: Vec<String>,
    pub arg0: Option<String>,
    pub cwd: String,
    pub env: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
    pub history: usize,
    pub log: String,
    /// Snapshot file whose history is restored before the child starts.
    pub snapshot_in: Option<String>,
    /// Text fed into the emulator before the child starts (restore banner).
    pub banner: Option<String>,
    /// Keys typed into the child shortly after start (e.g. agent resume).
    pub type_in: Option<String>,
    /// File of terminal output replayed before the child starts (history
    /// migrated from tmux); deleted once read.
    #[serde(default)]
    pub feed_in: Option<String>,
    /// While the pane shows a tmux pane live (`amux __tmux-bridge`): that
    /// pane's process and start time. Foreground-process reports then
    /// describe its terminal (so its agents are recognised).
    #[serde(default)]
    pub proc_of: Option<(u32, u64)>,
}

const FRAME_INTERVAL: Duration = Duration::from_millis(8);
const SYNC_MAX: Duration = Duration::from_millis(150);
const PROC_DELAY: Duration = Duration::from_millis(250);
const LINGER: Duration = Duration::from_secs(24 * 3600);
const MAX_INQ: usize = 4 << 20;

#[derive(Clone, Copy, Default, PartialEq)]
struct View {
    cursor: (usize, usize),
    pending: bool,
    modes: (u32, u8, u16, u16),
}

struct Keeper {
    spec: Spec,
    master: OwnedFd,
    child: u32,
    status: Option<i32>,
    exit_sent: bool,
    eof: bool,
    emu: Emulator,
    copy: CopyMode,
    listener: UnixListener,
    conn: Option<Conn>,
    hello: bool,
    credit: bool,
    need_full: bool,
    last_frame: Instant,
    /// Cursor and modes as last sent: a change to them alone (a program
    /// moving the cursor, or asking for bracketed paste) needs a frame too.
    sent_view: View,
    sent_images: Vec<(u64, u16, u16, u16, u16)>,
    sent_image_ids: HashSet<u64>,
    inq: Vec<u8>,
    proc_last: Option<(u32, String, String)>,
    proc_due: Option<Instant>,
    sync_since: Option<Instant>,
    started: u64,
    linger_until: Option<Instant>,
    type_in_at: Option<Instant>,
    recoveries: u32,
    copy_was_active: bool,
    closing: bool,
    rbuf: Box<[u8]>,
    /// AMUX_PROF=1: log phase timings every few seconds.
    prof_at: Option<Instant>,
}

pub fn main(spec_json: &str) -> i32 {
    let spec: Spec = match serde_json::from_str(spec_json) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("amux keeper: bad spec: {}", e);
            return 2;
        }
    };
    util::log_init(std::path::Path::new(&spec.log), &format!("keeper{}", spec.id));
    util::install_panic_log();
    match run(spec) {
        Ok(()) => 0,
        Err(e) => {
            logf!("keeper exiting: {}", e);
            1
        }
    }
}

fn listener_from_fd3_or_bind(path: &str) -> io::Result<UnixListener> {
    // fd 3 is the pre-bound listening socket passed by the server.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fstat on fd 3; failure just means it was not passed.
    if unsafe { libc::fstat(3, &mut st) } == 0 && (st.st_mode & libc::S_IFMT) == libc::S_IFSOCK {
        util::set_cloexec(3, true)?;
        // SAFETY: fd 3 is a socket we own from now on.
        let l = unsafe { UnixListener::from_raw_fd(3) };
        l.set_nonblocking(true)?;
        return Ok(l);
    }
    let _ = std::fs::remove_file(path);
    let l = UnixListener::bind(path)?;
    l.set_nonblocking(true)?;
    Ok(l)
}

fn run(spec: Spec) -> io::Result<()> {
    util::ignore_signal(libc::SIGPIPE);
    util::ignore_signal(libc::SIGHUP);
    util::ignore_signal(libc::SIGINT);
    util::ignore_signal(libc::SIGQUIT);
    let sigs = util::Signals::install(&[libc::SIGCHLD, libc::SIGTERM])?;
    let listener = listener_from_fd3_or_bind(&spec.sock)?;
    let (master, slave) = pty::open(spec.cols.max(2), spec.rows.max(1))?;
    let mut emu = Emulator::new(spec.cols.max(2) as usize, spec.rows.max(1) as usize, spec.history);
    if let Some(snap) = &spec.snapshot_in
        && let Ok(data) = std::fs::read(snap)
    {
        emu.term.load_snapshot(&data);
    }
    if let Some(f) = &spec.feed_in {
        if let Ok(data) = std::fs::read(f) {
            emu.feed(&data);
        }
        let _ = std::fs::remove_file(f);
    }
    if let Some(b) = &spec.banner {
        emu.feed(b.as_bytes());
    }
    let child = pty::spawn(&slave, &pty::SpawnSpec { argv: &spec.argv, arg0: spec.arg0.as_deref(), cwd: &spec.cwd, env: &spec.env })?;
    drop(slave);
    util::set_nonblocking(master.as_raw_fd(), true)?;
    logf!("started pane {} pid {} argv {:?}", spec.id, child, spec.argv);
    let type_in_at = spec.type_in.as_ref().map(|_| Instant::now() + Duration::from_millis(600));
    let mut k = Keeper {
        spec,
        master,
        child,
        status: None,
        exit_sent: false,
        eof: false,
        emu,
        copy: CopyMode::new(),
        listener,
        conn: None,
        hello: false,
        credit: false,
        need_full: true,
        last_frame: Instant::now() - FRAME_INTERVAL,
        sent_view: View::default(),
        sent_images: Vec::new(),
        sent_image_ids: HashSet::new(),
        inq: Vec::new(),
        proc_last: None,
        proc_due: Some(Instant::now() + PROC_DELAY),
        sync_since: None,
        started: util::now_unix(),
        linger_until: None,
        type_in_at,
        recoveries: 0,
        copy_was_active: false,
        closing: false,
        rbuf: vec![0u8; 16384].into_boxed_slice(),
        prof_at: std::env::var_os("AMUX_PROF").map(|_| Instant::now()),
    };
    k.run(&sigs);
    let _ = std::fs::remove_file(&k.spec.sock);
    Ok(())
}

impl Keeper {
    fn run(&mut self, sigs: &util::Signals) {
        loop {
            if self.closing {
                return;
            }
            if let Some(l) = self.linger_until
                && Instant::now() >= l
            {
                logf!("linger expired; exiting");
                return;
            }
            let mut fds = vec![util::pfd(sigs.fd(), util::POLLIN), util::pfd(self.listener.as_raw_fd(), util::POLLIN)];
            let pty_idx = fds.len();
            let mut pev = 0;
            if !self.eof {
                pev |= util::POLLIN;
            }
            if !self.inq.is_empty() && !self.eof {
                pev |= util::POLLOUT;
            }
            fds.push(util::pfd(if pev == 0 { -1 } else { self.master.as_raw_fd() }, pev));
            let conn_idx = fds.len();
            if let Some(c) = &self.conn {
                let mut e = util::POLLIN;
                if c.pending_out() > 0 {
                    e |= util::POLLOUT;
                }
                fds.push(util::pfd(c.fd(), e));
            }
            let timeout = util::timeout_until(self.next_deadline());
            if util::poll(&mut fds, timeout).is_err() {
                continue;
            }
            let _iter = util::prof::time(7);
            if let Some(t) = self.prof_at
                && Instant::now() >= t
            {
                self.prof_at = Some(Instant::now() + Duration::from_secs(5));
                logf!("prof: {}", util::prof::report_with(&util::prof::KEEPER_NAMES).join(" | "));
            }
            if fds[0].revents != 0 {
                for s in sigs.drain() {
                    match s {
                        libc::SIGCHLD => self.reap(),
                        libc::SIGTERM => {
                            logf!("SIGTERM; hanging up pane");
                            self.hangup();
                            return;
                        }
                        _ => {}
                    }
                }
            }
            if fds[1].revents != 0 {
                self.accept();
            }
            let pr = fds[pty_idx].revents;
            if pr & util::POLLIN != 0 || (pr & util::POLLERR != 0 && !self.eof) {
                self.read_pty();
            }
            if pr & util::POLLOUT != 0 {
                self.flush_pty();
            }
            if conn_idx < fds.len() && self.conn.is_some() {
                let r = fds[conn_idx].revents;
                if r & (util::POLLIN | util::POLLERR) != 0 {
                    self.read_conn();
                }
            }
            self.timers();
            self.maybe_frame();
            self.flush_conn();
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        let mut d = self.proc_due;
        d = util::earliest(d, self.linger_until);
        d = util::earliest(d, self.type_in_at);
        if self.status.is_some() && !self.exit_sent && self.hello {
            d = util::earliest(d, Some(Instant::now() + Duration::from_millis(50)));
        }
        if self.hello && self.credit && self.dirty() {
            let next = self.last_frame + FRAME_INTERVAL;
            let next = match self.sync_since {
                Some(s) if self.emu.term.modes.sync => next.max(s + SYNC_MAX),
                _ => next,
            };
            d = util::earliest(d, Some(next));
        }
        d
    }

    fn current_images(&self) -> &[crate::term::Image] {
        if self.copy.active { &[] } else { &self.emu.term.images }
    }

    fn images_dirty(&self) -> bool {
        let cur = self.current_images();
        cur.len() != self.sent_images.len() || cur.iter().zip(&self.sent_images).any(|(i, s)| (i.id, i.col, i.row, i.w, i.h) != *s)
    }

    fn dirty(&self) -> bool {
        self.need_full || self.emu.term.is_dirty() || (self.copy.active && self.copy.dirty) || self.view() != self.sent_view || self.images_dirty()
    }

    fn view(&self) -> View {
        let t = &self.emu.term;
        let m = &t.modes;
        View { cursor: t.cursor(), pending: t.cursor_pending(), modes: (m.to_bits(), m.cursor_shape, m.mouse, m.mouse_enc) }
    }

    fn reap(&mut self) {
        for (pid, st) in pty::reap() {
            if pid == self.child {
                logf!("child {} exited status {}", pid, st);
                self.status = Some(st);
            }
        }
    }

    fn hangup(&mut self) {
        // SAFETY: signalling the child's process group (it is a session leader).
        unsafe {
            libc::kill(-(self.child as i32), libc::SIGHUP);
            libc::kill(self.child as i32, libc::SIGHUP);
        }
    }

    fn accept(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((s, _)) => {
                    // A newer server replaces any previous connection.
                    match Conn::new(s) {
                        Ok(c) => {
                            logf!("server connected");
                            self.conn = Some(c);
                            self.hello = false;
                            self.credit = false;
                            self.sent_images.clear();
                            self.sent_image_ids.clear();
                            self.linger_until = None;
                        }
                        Err(e) => logf!("accept setup failed: {}", e),
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
    }

    fn read_pty(&mut self) {
        let _t = util::prof::time(0);
        let mut buf = std::mem::take(&mut self.rbuf);
        let mut total = 0usize;
        loop {
            match rustix::io::read(&self.master, &mut buf[..]) {
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(n) => {
                    self.feed(&buf[..n]);
                    total += n;
                    if total >= 1 << 20 {
                        break;
                    }
                }
                Err(rustix::io::Errno::AGAIN) => break,
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => {
                    self.eof = true;
                    break;
                }
            }
        }
        self.rbuf = buf;
        if self.eof {
            self.reap();
            if self.status.is_none() {
                // Give the child a moment; status arrives via SIGCHLD.
                self.proc_due = Some(Instant::now() + Duration::from_millis(20));
            }
        }
    }

    fn feed(&mut self, data: &[u8]) {
        let _t = util::prof::time(1);
        let r = catch_unwind(AssertUnwindSafe(|| self.emu.feed(data)));
        if r.is_err() {
            self.recover(data);
        }
        if !self.emu.term.out.is_empty() {
            let out = std::mem::take(&mut self.emu.term.out);
            self.queue_pty(&out);
        }
        if !self.emu.term.events.is_empty() {
            let evs = std::mem::take(&mut self.emu.term.events);
            for e in evs {
                self.send_event(e);
            }
        }
        if self.emu.term.modes.sync {
            if self.sync_since.is_none() {
                self.sync_since = Some(Instant::now());
            }
        } else {
            self.sync_since = None;
        }
        if self.proc_due.is_none() {
            self.proc_due = Some(Instant::now() + PROC_DELAY);
        }
    }

    /// The emulator panicked: rebuild it, salvaging visible text, and ask the
    /// application to redraw. The program in the pane never notices.
    fn recover(&mut self, data: &[u8]) {
        self.recoveries += 1;
        logf!("emulator panic #{} on {} bytes; recovering", self.recoveries, data.len());
        let crash = std::path::Path::new(&self.spec.log).with_file_name(format!("crash-pane{}-{}.bin", self.spec.id, util::now_unix()));
        let _ = std::fs::write(&crash, data);
        let (cols, rows) = (self.emu.term.cols(), self.emu.term.rows());
        let salvage = catch_unwind(AssertUnwindSafe(|| self.emu.term.snapshot())).ok();
        let mut fresh = Emulator::new(cols, rows, self.spec.history);
        if let Some(s) = salvage {
            let _ = catch_unwind(AssertUnwindSafe(|| fresh.term.load_snapshot(&s)));
        }
        fresh.term.default_colors = self.emu.term.default_colors;
        self.emu = fresh;
        self.copy.exit();
        self.need_full = true;
        // Nudge full-screen apps to repaint.
        let _ = pty::set_size(&self.master, cols as u16, rows.saturating_sub(1).max(1) as u16);
        let _ = pty::set_size(&self.master, cols as u16, rows as u16);
        self.send_event_raw(ev::RECOVERED, &Enc::new().u32(self.recoveries).buf);
    }

    fn queue_pty(&mut self, b: &[u8]) {
        if self.eof {
            return;
        }
        if self.inq.len() + b.len() > MAX_INQ {
            logf!("input queue full; dropping {} bytes", b.len());
            return;
        }
        self.inq.extend_from_slice(b);
        self.flush_pty();
    }

    fn flush_pty(&mut self) {
        while !self.inq.is_empty() {
            match rustix::io::write(&self.master, &self.inq) {
                Ok(n) => {
                    self.inq.drain(..n);
                }
                Err(rustix::io::Errno::AGAIN) => break,
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => {
                    self.inq.clear();
                    break;
                }
            }
        }
    }

    fn send(&mut self, ty: u8, payload: &[u8]) {
        if let Some(c) = self.conn.as_mut()
            && self.hello
        {
            c.send(ty, payload);
        }
    }

    fn send_event_raw(&mut self, kind: u8, payload: &[u8]) {
        let mut e = Enc::new();
        e.u8(kind);
        e.buf.extend_from_slice(payload);
        self.send(k2s::EVENT, &e.buf);
    }

    fn send_event(&mut self, e: Event) {
        let mut p = Enc::new();
        let kind = match &e {
            Event::Bell => ev::BELL,
            Event::Title(t) => {
                p.str(t);
                ev::TITLE
            }
            Event::Cwd(c) => {
                p.str(c);
                ev::CWD
            }
            Event::Notify(t, b) => {
                p.str(t).str(b);
                ev::NOTIFY
            }
            Event::Progress(s, v) => {
                p.u8(*s).u8(*v);
                ev::PROGRESS
            }
            Event::Clipboard(b) => {
                p.str(b);
                ev::CLIPBOARD
            }
            Event::Prompt(k, code) => {
                p.u8(*k).u8(code.is_some() as u8).i32(code.unwrap_or(0));
                ev::PROMPT
            }
        };
        self.send_event_raw(kind, &p.buf);
    }

    fn read_conn(&mut self) {
        let _t = util::prof::time(4);
        let Some(c) = self.conn.as_mut() else { return };
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
            if let Err(e) = self.handle(ty, &payload) {
                logf!("bad message {}: {}", ty, e);
            }
        }
        if eof {
            logf!("server disconnected");
            self.conn = None;
            self.hello = false;
            if self.status.is_some() {
                self.linger_until = Some(Instant::now() + LINGER);
            }
        }
    }

    fn handle(&mut self, ty: u8, payload: &[u8]) -> Result<(), proto::Bad> {
        let mut d = Dec::new(payload);
        match ty {
            s2k::HELLO => {
                let _version = d.u32()?;
                self.hello = true;
                self.credit = true;
                self.need_full = true;
                self.sent_images.clear();
                self.sent_image_ids.clear();
                self.send_info();
                self.proc_last = None;
                self.check_proc();
                if self.status.is_some() {
                    self.exit_sent = false;
                }
            }
            s2k::INPUT => {
                let b = d.rest().to_vec();
                if self.status.is_none() {
                    self.queue_pty(&b);
                }
                if self.proc_due.is_none() {
                    self.proc_due = Some(Instant::now() + PROC_DELAY);
                }
            }
            s2k::RESIZE => {
                let cols = d.u16()?.max(2);
                let rows = d.u16()?.max(1);
                if (cols as usize, rows as usize) != (self.emu.term.cols(), self.emu.term.rows()) {
                    let r = catch_unwind(AssertUnwindSafe(|| self.emu.term.resize(cols as usize, rows as usize)));
                    if r.is_err() {
                        self.recover(&[]);
                        let _ = catch_unwind(AssertUnwindSafe(|| self.emu.term.resize(cols as usize, rows as usize)));
                    }
                    let _ = pty::set_size(&self.master, cols, rows);
                    self.copy.sync(&self.emu.term);
                    self.need_full = true;
                }
            }
            s2k::ACK => self.credit = true,
            s2k::REDRAW => self.need_full = true,
            s2k::CAPTURE => {
                let req = d.u32()?;
                let start = d.i32()?;
                let end = d.i32()?;
                let flags = d.u32()?;
                let text = self.capture(start, end, flags);
                let mut e = Enc::new();
                e.u32(req).str(&text);
                self.send(k2s::CAPTURED, &e.buf);
            }
            s2k::COPY => {
                let args = d.strs()?;
                self.copy_cmd(&args);
            }
            s2k::SIGNAL => {
                let sig = d.i32()?;
                // SAFETY: signalling our child's process group.
                unsafe {
                    libc::kill(-(self.child as i32), sig);
                }
            }
            s2k::CLOSE => {
                logf!("close requested");
                self.hangup();
                self.closing = true;
            }
            s2k::SNAPSHOT => {
                let path = d.string()?;
                let snap = self.emu.term.snapshot();
                if let Err(e) = util::write_atomic(std::path::Path::new(&path), &snap) {
                    logf!("snapshot {} failed: {}", path, e);
                }
            }
            s2k::SETOPT => {
                let key = d.string()?;
                let val = d.string()?;
                self.set_option(&key, &val);
            }
            s2k::CLEAR_HISTORY => {
                self.emu.term.hist.clear();
                self.copy.sync(&self.emu.term);
            }
            s2k::PANIC_TEST => {
                // Exercise the recovery path deliberately.
                let r = catch_unwind(AssertUnwindSafe(|| {
                    panic!("injected emulator panic");
                }));
                if r.is_err() {
                    self.recover(b"<injected>");
                }
            }
            s2k::ECHO => {
                // Render text into the pane without sending it to the program.
                let b = d.rest().to_vec();
                let _ = catch_unwind(AssertUnwindSafe(|| self.emu.feed(&b)));
                self.emu.term.out.clear();
            }
            _ => {}
        }
        Ok(())
    }

    fn set_option(&mut self, key: &str, val: &str) {
        match key {
            "history-limit" => {
                if let Ok(n) = val.parse::<usize>() {
                    self.spec.history = n;
                    self.emu.term.hist.set_limit(n);
                }
            }
            "colors" => {
                let parse = |s: &str| -> Option<(u8, u8, u8)> {
                    let v: Vec<u8> = s.split(',').filter_map(|x| x.trim().parse().ok()).collect();
                    (v.len() == 3).then(|| (v[0], v[1], v[2]))
                };
                if let Some((f, b)) = val.split_once(';')
                    && let (Some(f), Some(b)) = (parse(f), parse(b))
                {
                    self.emu.term.default_colors = Some((f, b));
                }
            }
            _ => {}
        }
    }

    fn copy_cmd(&mut self, args: &[String]) {
        let Some(first) = args.first() else { return };
        match first.as_str() {
            "enter" => {
                let scroll_exit = args.get(1).is_some_and(|s| s == "1");
                self.copy.enter(&self.emu.term, scroll_exit);
                if let Some(n) = args.get(2).and_then(|s| s.parse::<isize>().ok()) {
                    let _ = self.copy.command(&self.emu.term, &["scroll-up".into(), n.to_string()]);
                }
            }
            "exit" => self.copy.exit(),
            _ => {
                let out = self.copy.command(&self.emu.term, args);
                match out {
                    Outcome::Copy(text, _cancel, pipe) => {
                        let mut e = Enc::new();
                        e.str(&text).str(pipe.as_deref().unwrap_or(""));
                        self.send(k2s::COPIED, &e.buf);
                    }
                    Outcome::Exit | Outcome::None => {}
                }
            }
        }
        self.need_full = true;
        self.send_mode_if_changed();
    }

    fn send_mode_if_changed(&mut self) {
        if self.copy.active != self.copy_was_active {
            self.copy_was_active = self.copy.active;
            let mut e = Enc::new();
            e.u8(self.copy.active as u8);
            self.send(k2s::MODE, &e.buf);
        }
    }

    /// Capture text: rows from `start` to `end` (0 = first screen row,
    /// negative = history). Flags: 1 join wrapped lines, 2 keep trailing spaces.
    fn capture(&mut self, start: i32, end: i32, flags: u32) -> String {
        let join = flags & 1 != 0;
        let t = &self.emu.term;
        let cols = t.cols();
        // Build physical rows for the needed part of history (newest first).
        let need_hist = if start < 0 { (-(start as i64)) as usize } else { 0 };
        let mut hist_rows: Vec<(String, bool)> = Vec::new();
        if need_hist > 0 {
            let mut cl = Clusters::default();
            for l in t.hist.iter().rev() {
                let cells = crate::history::cellize(crate::history::runs(l), &mut cl);
                let rows = crate::history::wrap_cells(&cells, cols);
                for (cells, wrapped) in rows.into_iter().rev() {
                    let mut s = String::new();
                    for c in &cells {
                        push_cell_text(&mut s, c, &cl);
                    }
                    hist_rows.push((s, wrapped));
                }
                if hist_rows.len() >= need_hist {
                    break;
                }
            }
            hist_rows.truncate(need_hist);
            hist_rows.reverse();
        }
        let hn = hist_rows.len() as i64;
        let mut lines: Vec<(String, bool)> = hist_rows;
        let screen_rows = t.rows() as i64;
        for y in 0..t.rows() {
            let r = t.row(y);
            let mut s = String::new();
            for c in &r.cells {
                push_cell_text(&mut s, c, &t.clusters);
            }
            lines.push((s, r.wrapped));
        }
        let first = (start as i64).max(-hn) + hn;
        let last = (end as i64).min(screen_rows - 1) + hn;
        let mut out = String::new();
        let mut i = first.max(0);
        while i <= last && (i as usize) < lines.len() {
            let (s, wrapped) = &lines[i as usize];
            if join && *wrapped && i < last {
                out.push_str(s);
            } else {
                out.push_str(if flags & 2 != 0 { s } else { s.trim_end() });
                out.push('\n');
            }
            i += 1;
        }
        out
    }

    fn send_info(&mut self) {
        let mut e = Enc::new();
        e.u32(self.spec.id)
            .u32(std::process::id())
            .u32(self.child)
            .u16(self.emu.term.cols() as u16)
            .u16(self.emu.term.rows() as u16)
            .u8(self.status.is_some() as u8)
            .i32(self.status.unwrap_or(0))
            .str(&self.emu.term.title)
            .str(&self.spec.cwd)
            .strs(&self.spec.argv)
            .u64(self.started)
            .u32(self.recoveries);
        self.send(k2s::INFO, &e.buf);
    }

    fn check_proc(&mut self) {
        let _t = util::prof::time(5);
        self.proc_due = None;
        if self.status.is_some() {
            return;
        }
        let mut pg = pty::fg_pgrp(&self.master).unwrap_or(self.child);
        if let Some((pid, ticks)) = self.spec.proc_of {
            let bridging = pg == self.child && util::proc_cmdline(pg).is_some_and(|a| a.iter().any(|w| w == "__tmux-bridge"));
            if let Some(st) = util::proc_stat(pid).filter(|st| bridging && st.start_ticks == ticks) {
                pg = st.tpgid.unwrap_or(pid);
            } else if !bridging {
                // The bridge has ended (it became a shell): report this pane.
                self.spec.proc_of = None;
            }
        }
        // The command line of a running process does not change: only read
        // it when the foreground process group does.
        let cmd = match &self.proc_last {
            Some((last_pg, last_cmd, _)) if *last_pg == pg => last_cmd.clone(),
            _ => util::proc_cmdline(pg).map(|v| v.join(" ")).unwrap_or_default().chars().take(512).collect(),
        };
        let cwd = util::proc_cwd(pg).or_else(|| util::proc_cwd(self.child)).unwrap_or_default();
        let cur = (pg, cmd, cwd);
        if self.proc_last.as_ref() != Some(&cur) {
            let mut e = Enc::new();
            e.u32(cur.0).str(&cur.1).str(&cur.2);
            self.send(k2s::PROC, &e.buf);
            self.proc_last = Some(cur);
        }
    }

    fn timers(&mut self) {
        let _t = util::prof::time(6);
        let now = Instant::now();
        if let Some(t) = self.type_in_at
            && now >= t
        {
            self.type_in_at = None;
            if let Some(s) = self.spec.type_in.clone() {
                self.queue_pty(s.as_bytes());
            }
        }
        if let Some(d) = self.proc_due
            && now >= d
        {
            if self.eof && self.status.is_none() {
                self.reap();
            }
            self.check_proc();
        }
        if let Some(st) = self.status
            && !self.exit_sent
            && self.hello
        {
            // Drain remaining output first so the final screen is complete.
            if self.eof || now.duration_since(self.last_frame) > Duration::from_millis(200) {
                let mut e = Enc::new();
                e.i32(st);
                self.send(k2s::EXITED, &e.buf);
                self.exit_sent = true;
            }
        }
        if self.status.is_some() && self.conn.is_none() && self.linger_until.is_none() {
            self.linger_until = Some(now + LINGER);
        }
    }

    fn maybe_frame(&mut self) {
        let _t = util::prof::time(2);
        if !self.hello || !self.credit || !self.dirty() {
            return;
        }
        let now = Instant::now();
        if now < self.last_frame + FRAME_INTERVAL {
            return;
        }
        if self.emu.term.modes.sync
            && let Some(s) = self.sync_since
            && now < s + SYNC_MAX
        {
            return;
        }
        let frame = catch_unwind(AssertUnwindSafe(|| self.build_frame()));
        match frame {
            Ok(f) => {
                self.send(k2s::FRAME, &f);
                if self.images_dirty() {
                    let msg = self.build_images();
                    self.send(k2s::IMAGES, &msg);
                }
                self.sent_view = self.view();
                self.credit = false;
                self.last_frame = now;
                self.need_full = false;
            }
            Err(_) => self.recover(&[]),
        }
    }

    fn build_images(&mut self) -> Vec<u8> {
        let imgs: Vec<crate::term::Image> = self.current_images().to_vec();
        let mut e = Enc::new();
        e.var(imgs.len() as u64);
        for img in &imgs {
            let send_data = self.sent_image_ids.insert(img.id);
            e.u64(img.id).u16(img.col).u16(img.row).u16(img.w).u16(img.h).u8(img.preserve_aspect as u8).u8(send_data as u8);
            if send_data {
                e.str(&img.b64);
            }
        }
        self.sent_image_ids.retain(|id| imgs.iter().any(|i| i.id == *id));
        self.sent_images = imgs.iter().map(|i| (i.id, i.col, i.row, i.w, i.h)).collect();
        e.buf
    }

    fn build_frame(&mut self) -> Vec<u8> {
        let t = &mut self.emu.term;
        let cols = t.cols();
        let rows = t.rows();
        let copy = self.copy.active;
        let full = self.need_full || t.all_dirty || copy;
        let mut e = Enc::new();
        e.buf.reserve(if full { rows * (cols * 12 + 8) + 256 } else { 4096 });
        let (cx, cy, visible) = if copy { (self.copy.cx, self.copy.cy, true) } else { (t.cursor().0, t.cursor().1, t.modes.cursor_visible) };
        let flags = (full as u8) | ((copy as u8) << 1);
        e.u8(flags)
            .u16(cols as u16)
            .u16(rows as u16)
            .u16(cx as u16)
            .u16(cy as u16)
            .u8(visible as u8 | ((t.cursor_pending() as u8) << 1))
            .u32(t.modes.to_bits())
            .u8(t.modes.cursor_shape)
            .u16(t.modes.mouse)
            .u16(t.modes.mouse_enc)
            .str(&t.title);
        let mut body = Enc::new();
        let mut clusters: Vec<(u32, String)> = Vec::new();
        let mut seen: HashSet<u32> = HashSet::new();
        let mut nrows = 0u64;
        let mut put_row = |y: usize, cells: &[Cell], wrapped: bool, table: &Clusters, body: &mut Enc| {
            let mut n = cells.len();
            while n > 0 && cells[n - 1] == Cell::EMPTY {
                n -= 1;
            }
            body.u16(y as u16).u8(wrapped as u8).var(n as u64);
            for c in &cells[..n] {
                body.u32(c.ch).u32(c.fg).u32(c.bg);
                if c.ch & CLUSTER_BIT != 0 && seen.insert(c.ch) {
                    clusters.push((c.ch, table.get(c.ch).unwrap_or(" ").to_string()));
                }
            }
            nrows += 1;
        };
        if copy {
            let view = self.copy.render(t);
            for (y, (cells, wrapped)) in view.iter().enumerate() {
                put_row(y, cells, *wrapped, &self.copy.clusters, &mut body);
            }
            t.take_dirty();
        } else {
            let ys: Vec<usize> = if full {
                t.take_dirty();
                (0..rows).collect()
            } else {
                t.take_dirty()
            };
            for y in ys {
                let r = t.row(y);
                put_row(y, &r.cells, r.wrapped, &t.clusters, &mut body);
            }
        }
        e.var(clusters.len() as u64);
        for (id, s) in &clusters {
            e.u32(*id).str(s);
        }
        e.var(nrows);
        e.buf.extend_from_slice(&body.buf);
        e.buf
    }

    fn flush_conn(&mut self) {
        let _t = util::prof::time(3);
        if let Some(c) = self.conn.as_mut()
            && c.flush().is_err()
        {
            logf!("server write failed; dropping connection");
            self.conn = None;
            self.hello = false;
        }
    }
}

/// Decoded frame (used by the server mirror and by tests).
pub struct Frame {
    pub full: bool,
    pub copy: bool,
    pub cols: usize,
    pub rows: usize,
    pub cx: usize,
    pub cy: usize,
    pub cursor_visible: bool,
    pub modes: crate::term::Modes,
    pub title: String,
    pub clusters: Vec<(u32, String)>,
    pub lines: Vec<(usize, bool, Vec<Cell>)>,
}

pub fn decode_frame(b: &[u8]) -> Result<Frame, proto::Bad> {
    let mut d = Dec::new(b);
    let flags = d.u8()?;
    let cols = d.u16()? as usize;
    let rows = d.u16()? as usize;
    let cx = d.u16()? as usize;
    let cy = d.u16()? as usize;
    let cf = d.u8()?;
    let mbits = d.u32()?;
    let shape = d.u8()?;
    let mouse = d.u16()?;
    let menc = d.u16()?;
    let title = d.string()?;
    let nc = d.len()?;
    let mut clusters = Vec::with_capacity(nc.min(4096));
    for _ in 0..nc {
        let id = d.u32()?;
        clusters.push((id, d.string()?));
    }
    let nr = d.len()?;
    if cols == 0 || rows == 0 || cols > 4096 || rows > 4096 || nr > rows {
        return Err(proto::Bad);
    }
    let mut lines = Vec::with_capacity(nr);
    for _ in 0..nr {
        let y = d.u16()? as usize;
        let wrapped = d.u8()? != 0;
        let n = d.len()?;
        if y >= rows || n > cols {
            return Err(proto::Bad);
        }
        // Rows arrive with trailing empty cells trimmed; they stay that way.
        let mut cells = Vec::with_capacity(n);
        for _ in 0..n {
            cells.push(Cell { ch: d.u32()?, fg: d.u32()?, bg: d.u32()? });
        }
        lines.push((y, wrapped, cells));
    }
    Ok(Frame {
        full: flags & 1 != 0,
        copy: flags & 2 != 0,
        cols,
        rows,
        cx,
        cy,
        cursor_visible: cf & 1 != 0,
        modes: crate::term::Modes::from_bits(mbits, shape, mouse, menc),
        title,
        clusters,
        lines,
    })
}
