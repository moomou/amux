//! `amux __tmux-bridge -S SOCKET -t %PANE [--pid PID --start TICKS]`: a tmux
//! pane, live, in an amux pane.
//!
//! The pane's program keeps running in tmux. The bridge attaches to its
//! session as a tmux control-mode client, paints the pane's current screen
//! (and history), then streams its output and forwards what is typed. When
//! the pane or tmux goes away, the bridge becomes a normal shell in the
//! pane's last directory.
//!
//! Protecting tmux (behaviour of tmux 3.2a, control.c and server-client.c):
//! - The client attaches with `pause-after`: when it falls behind, tmux
//!   pauses the pane for this client only (the bridge then repaints) instead
//!   of buffering its output, which grows tmux's memory without bound.
//! - When control clients are the only clients attached and all are behind,
//!   tmux stops reading the pane, stalling its program. So the reader thread
//!   does nothing but read tmux: output for the amux pane goes through a
//!   bounded queue and is dropped (then repainted) if amux cannot keep up,
//!   and the reader never waits for the lock held while sending commands.
//! - The client never sets a size (no `refresh-client -C`), so tmux ignores
//!   it when sizing windows. Other panes' output is turned off for it.
//! - It attaches to the session, not the pane (that would select the pane's
//!   window in tmux), with `-E` (attaching would otherwise copy amux's
//!   `update-environment` variables into the session) and `-N` (never start
//!   a tmux server). It only sends keys someone types, and pastes through a
//!   temporary buffer that is deleted again.
//! - The tmux client dies with the bridge (`PR_SET_PDEATHSIG`). An orphaned
//!   control client with output still pending stays attached for good, and
//!   if it is the only client, tmux stops reading the pane: its program
//!   freezes.
//! - It only attaches to the pane it was made for: the same process
//!   (`--pid`), started at the same time (`--start`). A restarted tmux reuses
//!   pane ids.

use std::collections::{HashSet, VecDeque};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// History lines painted when the bridge starts.
const HISTORY: usize = 2000;
/// Output waiting for the amux pane, beyond which it is dropped (and the
/// screen repainted) rather than slowing tmux down.
const QUEUE_MAX: usize = 8 << 20;
/// How often the pane's existence is checked.
const TICK: Duration = Duration::from_secs(2);
const MIN_RESYNC: Duration = Duration::from_millis(100);
/// Bytes per `send-keys -H` command.
const KEYS_CHUNK: usize = 256;

const INFO: &str = "#{pane_width}\t#{pane_height}\t#{cursor_x}\t#{cursor_y}\t#{alternate_on}\t#{cursor_flag}\t#{insert_flag}\t\
#{keypad_cursor_flag}\t#{keypad_flag}\t#{mouse_standard_flag}\t#{mouse_button_flag}\t#{mouse_any_flag}\t#{mouse_sgr_flag}\t\
#{mouse_utf8_flag}\t#{origin_flag}\t#{wrap_flag}\t#{scroll_region_upper}\t#{scroll_region_lower}\t#{pane_current_path}\t#{pane_title}";

static WINCH: AtomicBool = AtomicBool::new(false);

extern "C" fn on_winch(_: libc::c_int) {
    WINCH.store(true, Ordering::Relaxed);
}

pub fn main(args: &[String]) -> i32 {
    let (mut socket, mut pane, mut pid, mut start) = (None, None, None, 0);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-S" => socket = it.next().cloned(),
            "-t" => pane = it.next().cloned(),
            "--pid" => pid = it.next().and_then(|p| p.parse::<u32>().ok()),
            "--start" => start = it.next().and_then(|t| t.parse::<u64>().ok()).unwrap_or(0),
            _ => {}
        }
    }
    let (Some(socket), Some(pane)) = (socket, pane) else {
        eprintln!("usage: amux __tmux-bridge -S SOCKET -t %PANE [--pid PID --start TICKS]");
        return 2;
    };
    let cwd = Arc::new(Mutex::new(std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default()));
    let mut raw = crate::util::RawMode::enter();
    crate::util::ignore_signal(libc::SIGPIPE);
    crate::util::ignore_signal(libc::SIGTSTP);
    // SAFETY: plain signal disposition; the handler only stores an atomic.
    unsafe {
        libc::signal(libc::SIGWINCH, on_winch as *const () as libc::sighandler_t);
    }
    let why = match run(&socket, &pane, pid.map(|p| (p, start)), &cwd) {
        Ok(w) | Err(w) => w,
    };
    raw.restore();
    let dir = cwd.lock().map(|c| c.clone()).unwrap_or_default();
    become_shell(&pane, &why, &dir)
}

/// The tmux pane is gone: carry on as a normal shell in its last directory.
fn become_shell(pane: &str, why: &str, dir: &str) -> i32 {
    // Leave whatever modes the tmux program had set.
    let reset = "\x1b[?1049l\x1b[r\x1b[0m\x1b[?25h\x1b[?1l\x1b>\x1b[?7h\x1b[4l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l";
    let msg = format!("{}\r\n\x1b[2m── tmux pane {}: {}; this is an amux shell now ──\x1b[0m\r\n", reset, pane, why);
    let _ = std::io::stdout().write_all(msg.as_bytes());
    let _ = std::io::stdout().flush();
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into());
    let base = shell.rsplit('/').next().unwrap_or("sh").to_string();
    let mut cmd = Command::new(&shell);
    cmd.arg0(format!("-{}", base));
    if std::path::Path::new(dir).is_dir() {
        cmd.current_dir(dir);
    }
    let err = cmd.exec();
    eprintln!("amux: cannot start {}: {}", shell, err);
    1
}

/// What a command's reply is for (replies come back in order).
enum Pending {
    Ignore,
    Info,
    Saved,
    Screen { history: bool },
    Panes,
    Remove(PathBuf),
}

enum Ev {
    /// Repaint (after a pause or dropped output); `cont` resumes a paused pane.
    Resync {
        cont: bool,
    },
    Recheck,
    Panes(Vec<(String, bool, String)>),
    Gone(String),
}

struct Shared {
    /// Commands to tmux. Held while writing; the reader never takes it.
    tmux_in: Mutex<Option<ChildStdin>>,
    /// Replies expected, in the order the commands were sent.
    pending: Mutex<VecDeque<Pending>>,
    out: Mutex<OutQ>,
    out_cv: Condvar,
    done: AtomicBool,
    pastes: AtomicU64,
}

#[derive(Default)]
struct OutQ {
    chunks: VecDeque<Vec<u8>>,
    bytes: usize,
}

impl Shared {
    /// Send a command; false if tmux is gone.
    fn send(&self, line: &str, p: Pending) -> bool {
        let Ok(mut w) = self.tmux_in.lock() else { return false };
        let Some(stdin) = w.as_mut() else { return false };
        if let Ok(mut q) = self.pending.lock() {
            q.push_back(p);
        }
        stdin.write_all(line.as_bytes()).and_then(|_| stdin.write_all(b"\n")).and_then(|_| stdin.flush()).is_ok()
    }

    /// Queue output for the amux pane; false (nothing queued) if full.
    fn push(&self, data: Vec<u8>) -> bool {
        let Ok(mut q) = self.out.lock() else { return false };
        if q.bytes + data.len() > QUEUE_MAX {
            return false;
        }
        q.bytes += data.len();
        q.chunks.push_back(data);
        self.out_cv.notify_one();
        true
    }

    /// Replace queued output with a fresh screen.
    fn replace(&self, data: Vec<u8>) {
        if let Ok(mut q) = self.out.lock() {
            q.chunks.clear();
            q.bytes = data.len();
            q.chunks.push_back(data);
            self.out_cv.notify_one();
        }
    }
}

/// Run a single tmux command (not in control mode).
fn tmux_once(socket: &str, args: &[&str]) -> Result<String, String> {
    let o = Command::new("tmux")
        .arg("-N")
        .arg("-S")
        .arg(socket)
        .args(args)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run tmux: {}", e))?;
    if !o.status.success() {
        return Err(String::from_utf8_lossy(&o.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// `expect`: the pane's process and its start time, when known.
fn run(socket: &str, pane: &str, expect: Option<(u32, u64)>, cwd: &Mutex<String>) -> Result<String, String> {
    let found = tmux_once(socket, &["display-message", "-p", "-t", pane, "#{session_id} #{pane_pid}"]).map_err(|_| "it is gone".to_string())?;
    let (sid, pid) = found.split_once(' ').unwrap_or((found.as_str(), ""));
    let sid = sid.to_string();
    if !sid.starts_with('$') {
        return Err("it is gone".into());
    }
    if let Some((want, start)) = expect {
        let pid = pid.parse::<u32>().unwrap_or(0);
        if pid != want || (start != 0 && crate::util::proc_stat(pid).map_or(0, |s| s.start_ticks) != start) {
            return Err("it is gone (its id now belongs to another program)".into());
        }
    }
    let parent = std::process::id();
    let mut cmd = Command::new("tmux");
    cmd.args(["-N", "-S", socket, "-C", "attach-session", "-E", "-f", "ignore-size,pause-after=1", "-t", &sid])
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // SAFETY: only async-signal-safe calls, in the child before exec.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // The bridge died before the line above took effect.
            if libc::getppid() as u32 != parent {
                return Err(std::io::Error::other("the bridge is gone"));
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| format!("cannot run tmux: {}", e))?;
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Err("cannot talk to tmux".into());
    };
    let sh = Arc::new(Shared {
        tmux_in: Mutex::new(Some(stdin)),
        // The attach command's own reply comes first.
        pending: Mutex::new(VecDeque::from([Pending::Ignore])),
        out: Mutex::new(OutQ::default()),
        out_cv: Condvar::new(),
        done: AtomicBool::new(false),
        pastes: AtomicU64::new(0),
    });
    let (tx, rx) = mpsc::channel::<Ev>();
    {
        let (sh, tx, pane) = (sh.clone(), tx.clone(), pane.to_string());
        std::thread::spawn(move || guarded(&tx, || reader(stdout, &sh, &tx, &pane)));
    }
    let writer = {
        let (sh, tx) = (sh.clone(), tx.clone());
        std::thread::spawn(move || guarded(&tx, || writer(&sh)))
    };
    {
        let (sh, pane) = (sh.clone(), pane.to_string());
        std::thread::spawn(move || guarded(&tx, || input(&sh, &tx, &pane)));
    }
    let why = control(&sh, &rx, socket, pane, &sid, cwd);
    // Detach (an empty line), let pending output reach the pane, stop tmux.
    let _ = sh.send("", Pending::Ignore);
    if let Ok(mut w) = sh.tmux_in.lock() {
        *w = None;
    }
    sh.done.store(true, Ordering::SeqCst);
    sh.out_cv.notify_all();
    let t0 = Instant::now();
    while !writer.is_finished() && t0.elapsed() < Duration::from_millis(500) {
        std::thread::sleep(Duration::from_millis(10));
    }
    let t0 = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => break,
            Ok(None) if t0.elapsed() > Duration::from_secs(1) => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    Ok(why)
}

/// Run a bridge thread; if it fails, the bridge ends (and its tmux client
/// with it) rather than carrying on half-working.
fn guarded(ev: &Sender<Ev>, f: impl FnOnce()) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err() {
        let _ = ev.send(Ev::Gone("the bridge failed".into()));
    }
}

fn paint_request(sh: &Shared, pane: &str, history: bool, cont: bool) -> bool {
    if cont && !sh.send(&format!("refresh-client -A '{}:continue'", pane), Pending::Ignore) {
        return false;
    }
    let start = if history { format!(" -S -{}", HISTORY) } else { String::new() };
    sh.send(&format!("display-message -p -t {} '{}'", pane, INFO), Pending::Info)
        && sh.send(&format!("capture-pane -p -e -a -q -t {}", pane), Pending::Saved)
        && sh.send(&format!("capture-pane -p -e -t {}{}", pane, start), Pending::Screen { history })
}

/// Handle events until the pane is gone; returns why.
fn control(sh: &Shared, rx: &mpsc::Receiver<Ev>, _socket: &str, pane: &str, sid: &str, cwd: &Mutex<String>) -> String {
    let panes_cmd = format!("list-panes -s -t {} -F '#{{pane_id}} #{{pane_dead}} #{{pane_current_path}}'", sid);
    if !paint_request(sh, pane, true, false) || !sh.send(&panes_cmd, Pending::Panes) {
        return "tmux closed the connection".into();
    }
    let mut off: HashSet<String> = HashSet::new();
    let mut last_check = Instant::now();
    let mut last_resync: Option<Instant> = None;
    let mut resync_due: Option<bool> = None;
    loop {
        let ev = rx.recv_timeout(Duration::from_millis(100));
        if WINCH.swap(false, Ordering::Relaxed) {
            resync_due = Some(resync_due.unwrap_or(false));
        }
        match ev {
            Ok(Ev::Resync { cont }) => resync_due = Some(resync_due.unwrap_or(false) || cont),
            Ok(Ev::Recheck) => last_check = Instant::now() - TICK,
            Ok(Ev::Panes(list)) => {
                let Some((_, dead, path)) = list.iter().find(|(id, _, _)| id == pane) else {
                    return "it closed".into();
                };
                if *dead {
                    return "its program exited".into();
                }
                if let Ok(mut c) = cwd.lock()
                    && !path.is_empty()
                {
                    *c = path.clone();
                }
                for (id, _, _) in &list {
                    // `:pause` (not `:off`): in tmux 3.4+, `:off` on all control
                    // clients disables reading the pane when no terminal client
                    // is attached, whereas `:pause` drops output for this client
                    // only while keeping the pane's PTY reader active.
                    if id != pane && off.insert(id.clone()) && !sh.send(&format!("refresh-client -A '{}:pause'", id), Pending::Ignore) {
                        return "tmux closed the connection".into();
                    }
                }
            }
            Ok(Ev::Gone(why)) => return why,
            Err(RecvTimeoutError::Disconnected) => return "tmux closed the connection".into(),
            Err(RecvTimeoutError::Timeout) => {}
        }
        if let Some(cont) = resync_due
            && last_resync.is_none_or(|t| t.elapsed() >= MIN_RESYNC)
        {
            resync_due = None;
            last_resync = Some(Instant::now());
            if !paint_request(sh, pane, false, cont) {
                return "tmux closed the connection".into();
            }
        }
        if last_check.elapsed() >= TICK {
            last_check = Instant::now();
            if !sh.send(&panes_cmd, Pending::Panes) {
                return "tmux closed the connection".into();
            }
        }
    }
}

/// Read everything tmux sends, as fast as it comes.
fn reader(mut out: ChildStdout, sh: &Shared, ev: &Sender<Ev>, pane: &str) {
    let ours = format!("%output {} ", pane).into_bytes();
    let ours_ext = format!("%extended-output {} ", pane).into_bytes();
    let pause = format!("%pause {}", pane).into_bytes();
    let mut buf: Vec<u8> = Vec::with_capacity(1 << 16);
    let mut chunk = vec![0u8; 1 << 16];
    let mut block: Option<Vec<Vec<u8>>> = None;
    let mut streaming = false;
    let mut info: Option<Info> = None;
    let mut saved: Vec<Vec<u8>> = Vec::new();
    loop {
        let n = match out.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
        let mut start = 0;
        while let Some(off) = memchr::memchr(b'\n', &buf[start..]) {
            let line = &buf[start..start + off];
            start += off + 1;
            if let Some(lines) = block.as_mut() {
                if line.starts_with(b"%end ") || line.starts_with(b"%error ") {
                    let failed = line.starts_with(b"%error ");
                    let lines = block.take().unwrap_or_default();
                    let p = sh.pending.lock().ok().and_then(|mut q| q.pop_front()).unwrap_or(Pending::Ignore);
                    match p {
                        Pending::Ignore => {}
                        Pending::Remove(f) => {
                            let _ = std::fs::remove_file(f);
                        }
                        Pending::Info | Pending::Screen { .. } if failed => {
                            let _ = ev.send(Ev::Gone("it closed".into()));
                        }
                        Pending::Info => info = lines.first().and_then(|l| Info::parse(l)),
                        Pending::Saved => saved = if failed { Vec::new() } else { lines },
                        Pending::Screen { history } => {
                            // Without the pane's state, paint its text anyway.
                            let fallback = Info { h: lines.len().max(1) as u16, cursor: true, wrap: true, ..Default::default() };
                            let i = info.as_ref().unwrap_or(&fallback);
                            let rows = rustix::termios::tcgetwinsize(rustix::stdio::stdout()).map(|w| w.ws_row).unwrap_or(i.h);
                            sh.replace(paint(i, &saved, &lines, history, rows));
                            streaming = true;
                        }
                        Pending::Panes if failed => {
                            let _ = ev.send(Ev::Gone("its tmux session closed".into()));
                        }
                        Pending::Panes => {
                            let list = lines
                                .iter()
                                .filter_map(|l| {
                                    let s = String::from_utf8_lossy(l);
                                    let mut f = s.splitn(3, ' ');
                                    Some((f.next()?.to_string(), f.next()? == "1", f.next().unwrap_or("").to_string()))
                                })
                                .collect();
                            let _ = ev.send(Ev::Panes(list));
                        }
                    }
                } else {
                    lines.push(line.to_vec());
                }
                continue;
            }
            if line.starts_with(b"%begin ") {
                block = Some(Vec::new());
            } else if line.starts_with(&ours) || line.starts_with(&ours_ext) {
                if !streaming {
                    continue;
                }
                let data = if line.starts_with(&ours) {
                    &line[ours.len()..]
                } else {
                    let rest = &line[ours_ext.len()..];
                    match rest.windows(3).position(|w| w == b" : ") {
                        Some(i) => &rest[i + 3..],
                        None => continue,
                    }
                };
                if !sh.push(unescape(data)) {
                    // amux is not keeping up: drop it and repaint later.
                    streaming = false;
                    let _ = ev.send(Ev::Resync { cont: false });
                }
            } else if line == pause.as_slice() {
                streaming = false;
                let _ = ev.send(Ev::Resync { cont: true });
            } else if line.starts_with(b"%exit") {
                let _ = ev.send(Ev::Gone("tmux closed its session".into()));
            } else if line.starts_with(b"%layout-change")
                || line.starts_with(b"%window-add")
                || line.starts_with(b"%window-close")
                || line.starts_with(b"%unlinked-window-close")
                || line.starts_with(b"%session-changed")
                || line.starts_with(b"%sessions-changed")
            {
                let _ = ev.send(Ev::Recheck);
            }
        }
        buf.drain(..start);
    }
    let _ = ev.send(Ev::Gone("tmux closed the connection".into()));
}

/// Write queued output to the amux pane.
fn writer(sh: &Shared) {
    let mut out = std::io::stdout().lock();
    loop {
        let chunk = {
            let Ok(mut q) = sh.out.lock() else { return };
            loop {
                if let Some(c) = q.chunks.pop_front() {
                    q.bytes -= c.len();
                    break c;
                }
                if sh.done.load(Ordering::SeqCst) {
                    return;
                }
                q = match sh.out_cv.wait_timeout(q, Duration::from_millis(200)) {
                    Ok((q, _)) => q,
                    Err(_) => return,
                };
            }
        };
        if out.write_all(&chunk).and_then(|_| out.flush()).is_err() {
            return;
        }
    }
}

/// Forward what is typed: keys as raw bytes, pastes through a tmux buffer
/// (tmux then brackets them if the program asked for that).
fn input(sh: &Shared, ev: &Sender<Ev>, pane: &str) {
    const START: &[u8] = b"\x1b[200~";
    const END: &[u8] = b"\x1b[201~";
    let mut buf = [0u8; 4096];
    let mut paste: Option<Vec<u8>> = None;
    let mut carry: Vec<u8> = Vec::new();
    loop {
        // SAFETY: reading into a local buffer from stdin.
        let n = unsafe { libc::read(0, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            let _ = ev.send(Ev::Gone("the amux pane closed".into()));
            return;
        }
        let mut data = std::mem::take(&mut carry);
        data.extend_from_slice(&buf[..n as usize]);
        let mut rest: &[u8] = &data;
        while !rest.is_empty() {
            if let Some(p) = paste.as_mut() {
                match find(rest, END) {
                    Some(i) => {
                        p.extend_from_slice(&rest[..i]);
                        rest = &rest[i + END.len()..];
                        let text = paste.take().unwrap_or_default();
                        send_paste(sh, pane, &text);
                    }
                    None => {
                        // Keep a possible partial end marker for the next read.
                        let keep = (1..END.len()).rev().find(|&k| rest.len() >= k && END.starts_with(&rest[rest.len() - k..])).unwrap_or(0);
                        p.extend_from_slice(&rest[..rest.len() - keep]);
                        carry = rest[rest.len() - keep..].to_vec();
                        rest = &[];
                    }
                }
            } else {
                match find(rest, START) {
                    Some(i) => {
                        send_keys(sh, pane, &rest[..i]);
                        rest = &rest[i + START.len()..];
                        paste = Some(Vec::new());
                    }
                    None => {
                        send_keys(sh, pane, rest);
                        rest = &[];
                    }
                }
            }
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn send_keys(sh: &Shared, pane: &str, bytes: &[u8]) {
    for chunk in bytes.chunks(KEYS_CHUNK) {
        let hex: Vec<String> = chunk.iter().map(|b| format!("{:02x}", b)).collect();
        sh.send(&format!("send-keys -t {} -H {}", pane, hex.join(" ")), Pending::Ignore);
    }
}

fn send_paste(sh: &Shared, pane: &str, text: &[u8]) {
    if text.is_empty() {
        return;
    }
    let n = sh.pastes.fetch_add(1, Ordering::Relaxed);
    let name = format!("amux-bridge-{}-{}", std::process::id(), n);
    let file = std::env::temp_dir().join(format!("{}.paste", name));
    let written = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&file).and_then(|mut f| f.write_all(text));
    if written.is_err() {
        // Without a file, type it instead (unbracketed).
        send_keys(sh, pane, text);
        return;
    }
    let quoted = crate::util::shell_quote(&file.to_string_lossy());
    sh.send(&format!("load-buffer -b {} {}", name, quoted), Pending::Remove(file));
    sh.send(&format!("paste-buffer -p -d -b {} -t {}", name, pane), Pending::Ignore);
    // Had the paste failed (the pane just closed), the buffer would stay.
    sh.send(&format!("delete-buffer -b {}", name), Pending::Ignore);
}

/// Decode tmux's escaping of `%output` data (`\ooo` octal).
fn unescape(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'\\' && i + 4 <= s.len() && s[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            out.push(((s[i + 1] - b'0') << 6) | ((s[i + 2] - b'0') << 3) | (s[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    out
}

/// A pane's state, for painting it.
#[derive(Debug, Default, PartialEq)]
struct Info {
    w: u16,
    h: u16,
    cx: u16,
    cy: u16,
    alt: bool,
    cursor: bool,
    insert: bool,
    kcursor: bool,
    kpad: bool,
    m_std: bool,
    m_btn: bool,
    m_any: bool,
    m_sgr: bool,
    m_utf8: bool,
    origin: bool,
    wrap: bool,
    upper: u16,
    lower: u16,
    cwd: String,
    title: String,
}

impl Info {
    fn parse(line: &[u8]) -> Option<Info> {
        let s = String::from_utf8_lossy(line);
        let f: Vec<&str> = s.splitn(20, '\t').collect();
        if f.len() != 20 {
            return None;
        }
        let n = |v: &str| v.parse::<u16>().ok();
        let b = |v: &str| v == "1";
        Some(Info {
            w: n(f[0])?,
            h: n(f[1])?,
            cx: n(f[2])?,
            cy: n(f[3])?,
            alt: b(f[4]),
            cursor: b(f[5]),
            insert: b(f[6]),
            kcursor: b(f[7]),
            kpad: b(f[8]),
            m_std: b(f[9]),
            m_btn: b(f[10]),
            m_any: b(f[11]),
            m_sgr: b(f[12]),
            m_utf8: b(f[13]),
            origin: b(f[14]),
            wrap: b(f[15]),
            upper: n(f[16]).unwrap_or(0),
            lower: n(f[17]).unwrap_or(0),
            cwd: f[18].to_string(),
            title: f[19].to_string(),
        })
    }
}

/// Terminal output that recreates a tmux pane's screen: optionally its
/// history (into amux's scrollback), the normal screen, the alternate screen
/// if the program uses it, then the modes and the cursor.
fn paint(i: &Info, saved: &[Vec<u8>], screen: &[Vec<u8>], history: bool, rows_here: u16) -> Vec<u8> {
    let mut o: Vec<u8> = b"\x1b[?1049l\x1b[0m\x1b[r\x1b[?6l\x1b[H\x1b[2J".to_vec();
    let (normal, alt) = if i.alt { (saved, Some(screen)) } else { (screen, None) };
    let h = (i.h as usize).max(1);
    let (hist, vis) = normal.split_at(normal.len().saturating_sub(h));
    if history && !hist.is_empty() {
        for l in hist {
            o.extend_from_slice(l);
            o.extend_from_slice(b"\x1b[0m\r\n");
        }
        // Scroll what was printed into the history; the screen starts at the top.
        let used = hist.len().min(rows_here.saturating_sub(1) as usize);
        if used > 0 {
            o.extend_from_slice(format!("\x1b[{}S", used).as_bytes());
        }
        o.extend_from_slice(b"\x1b[H");
    }
    let lines = |o: &mut Vec<u8>, ls: &[Vec<u8>]| {
        for (k, l) in ls.iter().enumerate() {
            if k > 0 {
                o.extend_from_slice(b"\x1b[0m\r\n");
            }
            o.extend_from_slice(l);
        }
        o.extend_from_slice(b"\x1b[0m");
    };
    lines(&mut o, vis);
    if let Some(alt) = alt {
        o.extend_from_slice(b"\x1b[?1049h\x1b[H\x1b[2J");
        lines(&mut o, alt);
    }
    let flag = |o: &mut Vec<u8>, on: bool, code: &str| o.extend_from_slice(format!("\x1b[?{}{}", code, if on { 'h' } else { 'l' }).as_bytes());
    if i.upper != 0 || i.lower + 1 != i.h {
        o.extend_from_slice(format!("\x1b[{};{}r", i.upper + 1, i.lower + 1).as_bytes());
    }
    flag(&mut o, i.kcursor, "1");
    o.extend_from_slice(if i.kpad { b"\x1b=" } else { b"\x1b>" });
    flag(&mut o, i.wrap, "7");
    o.extend_from_slice(if i.insert { b"\x1b[4h" } else { b"\x1b[4l" });
    flag(&mut o, i.m_std, "1000");
    flag(&mut o, i.m_btn, "1002");
    flag(&mut o, i.m_any, "1003");
    flag(&mut o, i.m_sgr, "1006");
    flag(&mut o, i.m_utf8, "1005");
    // Pastes always arrive bracketed: tmux brackets them for the program only
    // if it asked (paste-buffer -p).
    flag(&mut o, true, "2004");
    let title: String = i.title.chars().filter(|c| !c.is_control()).collect();
    o.extend_from_slice(format!("\x1b]2;{}\x07", title).as_bytes());
    if i.origin {
        flag(&mut o, true, "6");
        o.extend_from_slice(format!("\x1b[{};{}H", i.cy.saturating_sub(i.upper) + 1, i.cx + 1).as_bytes());
    } else {
        o.extend_from_slice(format!("\x1b[{};{}H", i.cy + 1, i.cx + 1).as_bytes());
    }
    flag(&mut o, i.cursor, "25");
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescapes_control_mode_output() {
        assert_eq!(unescape(b"a\\033[31mb\\015\\012\\134x"), b"a\x1b[31mb\r\n\\x".to_vec());
        // Not an escape: left alone.
        assert_eq!(unescape(b"\\9x\\"), b"\\9x\\".to_vec());
        assert_eq!(unescape("é\\011".as_bytes()), "é\t".as_bytes().to_vec());
    }

    #[test]
    fn parses_pane_info() {
        let line = b"80\t24\t3\t5\t0\t1\t0\t1\t0\t0\t0\t0\t1\t0\t0\t1\t0\t23\t/home/u/my dir\tvim: x\ty";
        let i = Info::parse(line).unwrap();
        assert_eq!((i.w, i.h, i.cx, i.cy, i.kcursor, i.m_sgr, i.wrap, i.lower), (80, 24, 3, 5, true, true, true, 23));
        assert_eq!((i.cwd.as_str(), i.title.as_str()), ("/home/u/my dir", "vim: x\ty"));
        assert!(Info::parse(b"80\t24").is_none());
    }

    #[test]
    fn paints_history_then_the_screen_at_the_top() {
        let info = Info { w: 10, h: 2, cx: 1, cy: 1, cursor: true, wrap: true, lower: 1, ..Default::default() };
        let lines: Vec<Vec<u8>> = ["h1", "h2", "h3", "s1", "s2"].iter().map(|s| s.as_bytes().to_vec()).collect();
        let out = paint(&info, &[], &lines, true, 2);
        let mut e = crate::term::Emulator::new(10, 2, 100);
        e.feed(&out);
        let hist: Vec<String> = e.term.hist.iter().map(crate::history::line_text).collect();
        assert_eq!(hist, vec!["h1", "h2", "h3"]);
        assert_eq!(e.term.screen_text(), vec!["s1", "s2"]);
        assert_eq!(e.term.cursor(), (1, 1));
        // An alternate-screen program: the saved screen first, then its own.
        let alt = Info { alt: true, ..info };
        let out = paint(&alt, &[b"normal".to_vec(), b"".to_vec()], &[b"ALT".to_vec(), b"".to_vec()], false, 2);
        let mut e = crate::term::Emulator::new(10, 2, 100);
        e.feed(&out);
        assert_eq!(e.term.screen_text()[0], "ALT");
        e.feed(b"\x1b[?1049l");
        assert_eq!(e.term.screen_text()[0], "normal");
    }
}
