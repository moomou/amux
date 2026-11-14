//! Small OS helpers: signal self-pipe, poll, logging, atomic writes, base64.

use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static SIG_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(sig: libc::c_int) {
    let fd = SIG_WRITE_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let b = sig as u8;
        // SAFETY: write(2) is async-signal-safe; errors (EAGAIN) are ignored.
        unsafe {
            libc::write(fd, &b as *const u8 as *const libc::c_void, 1);
        }
    }
}

/// Self-pipe signal delivery: signals become readable bytes on `fd()`.
pub struct Signals {
    read: OwnedFd,
}

impl Signals {
    pub fn install(sigs: &[libc::c_int]) -> io::Result<Signals> {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: fds is a valid 2-element array.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: pipe2 returned two fresh descriptors we now own.
        let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        SIG_WRITE_FD.store(fds[1], Ordering::Relaxed);
        for &s in sigs {
            // SAFETY: installing a plain handler with an empty mask.
            unsafe {
                let mut sa: libc::sigaction = std::mem::zeroed();
                sa.sa_sigaction = on_signal as *const () as usize;
                sa.sa_flags = libc::SA_RESTART;
                libc::sigemptyset(&mut sa.sa_mask);
                libc::sigaction(s, &sa, std::ptr::null_mut());
            }
        }
        Ok(Signals { read })
    }

    pub fn fd(&self) -> RawFd {
        self.read.as_raw_fd()
    }

    pub fn drain(&self) -> Vec<libc::c_int> {
        let mut out = Vec::new();
        let mut buf = [0u8; 64];
        loop {
            // SAFETY: reading into a local buffer from our own pipe.
            let n = unsafe { libc::read(self.read.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n <= 0 {
                break;
            }
            out.extend(buf[..n as usize].iter().map(|&b| b as libc::c_int));
        }
        out
    }
}

pub fn ignore_signal(sig: libc::c_int) {
    // SAFETY: setting SIG_IGN is always valid.
    unsafe {
        libc::signal(sig, libc::SIG_IGN);
    }
}

pub fn default_signal(sig: libc::c_int) {
    // SAFETY: restoring the default disposition.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
    }
}

/// poll(2) over raw fds. Returns revents per entry.
pub fn poll(fds: &mut [libc::pollfd], timeout: Option<Duration>) -> io::Result<usize> {
    // Round up: truncating a sub-millisecond wait to 0 would busy-spin
    // until the deadline.
    let ms = match timeout {
        None => -1,
        Some(d) => d.as_nanos().div_ceil(1_000_000).min(i32::MAX as u128) as i32,
    };
    // SAFETY: fds points to a valid mutable slice of pollfd.
    let r = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
    if r >= 0 {
        return Ok(r as usize);
    }
    let e = io::Error::last_os_error();
    // A signal interrupted the wait: report no events (the signal pipe is
    // drained on the next iteration).
    if e.kind() == io::ErrorKind::Interrupted { Ok(0) } else { Err(e) }
}

pub fn pfd(fd: RawFd, events: i16) -> libc::pollfd {
    libc::pollfd { fd, events, revents: 0 }
}

pub const POLLIN: i16 = libc::POLLIN;
pub const POLLOUT: i16 = libc::POLLOUT;
pub const POLLERR: i16 = libc::POLLERR | libc::POLLHUP | libc::POLLNVAL;

pub fn set_nonblocking(fd: RawFd, on: bool) -> io::Result<()> {
    // SAFETY: fcntl on a caller-provided fd.
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        if fl < 0 {
            return Err(io::Error::last_os_error());
        }
        let nf = if on { fl | libc::O_NONBLOCK } else { fl & !libc::O_NONBLOCK };
        if libc::fcntl(fd, libc::F_SETFL, nf) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

pub fn set_cloexec(fd: RawFd, on: bool) -> io::Result<()> {
    // SAFETY: fcntl on a caller-provided fd.
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFD);
        let nf = if on { fl | libc::FD_CLOEXEC } else { fl & !libc::FD_CLOEXEC };
        if libc::fcntl(fd, libc::F_SETFD, nf) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Write a file atomically (tmp + rename).
pub fn write_atomic(path: &std::path::Path, data: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        // Durable before it replaces the old copy (survives power loss).
        f.sync_data()?;
    }
    std::fs::rename(&tmp, path)
}

/// Cheap always-on phase timers (shown by `amux server-info`).
pub mod prof {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    use std::time::Instant;

    pub const NAMES: [&str; 8] = ["keeper-msgs", "client-msgs", "timers", "agent-tick", "render-build", "render-diff", "save-state", "flush"];
    pub const KEEPER_NAMES: [&str; 8] = ["read-pty", "feed", "frame", "flush", "read-conn", "check-proc", "timers", "iterations"];
    pub const KEEPER: usize = 0;
    pub const CLIENT: usize = 1;
    pub const TIMERS: usize = 2;
    pub const AGENTS: usize = 3;
    pub const BUILD: usize = 4;
    pub const DIFF: usize = 5;
    pub const SAVE: usize = 6;
    pub const FLUSH: usize = 7;

    static NS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
    static CALLS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];

    pub struct Timer(usize, Instant);

    impl Drop for Timer {
        fn drop(&mut self) {
            NS[self.0].fetch_add(self.1.elapsed().as_nanos() as u64, Relaxed);
            CALLS[self.0].fetch_add(1, Relaxed);
        }
    }

    pub fn time(phase: usize) -> Timer {
        Timer(phase, Instant::now())
    }

    pub fn report() -> Vec<String> {
        report_with(&NAMES)
    }

    pub fn report_with(names: &[&str; 8]) -> Vec<String> {
        (0..names.len())
            .map(|i| {
                let (ns, n) = (NS[i].load(Relaxed), CALLS[i].load(Relaxed));
                format!(
                    "{:<13} {:>8} calls {:>10.2} ms total {:>8.1} us avg",
                    names[i],
                    n,
                    ns as f64 / 1e6,
                    if n > 0 { ns as f64 / n as f64 / 1e3 } else { 0.0 }
                )
            })
            .collect()
    }
}

static LOG_FILE: std::sync::Mutex<Option<std::fs::File>> = std::sync::Mutex::new(None);
static LOG_TAG: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

pub fn log_init(path: &std::path::Path, tag: &str) {
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    // Keep logs bounded: rotate past 4 MiB.
    if std::fs::metadata(path).map(|m| m.len() > 4 << 20).unwrap_or(false) {
        let _ = std::fs::rename(path, path.with_extension("log.1"));
    }
    if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        *LOG_FILE.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
    }
    *LOG_TAG.lock().unwrap_or_else(|e| e.into_inner()) = tag.to_string();
}

pub fn log(msg: &str) {
    let tag = LOG_TAG.lock().map(|t| t.clone()).unwrap_or_default();
    if let Ok(mut g) = LOG_FILE.lock()
        && let Some(f) = g.as_mut()
    {
        // One write per line: several processes append to the same log.
        let line = format!("{} [{}:{}] {}\n", now_ms(), tag, std::process::id(), msg);
        let _ = f.write_all(line.as_bytes());
    }
}

#[macro_export]
macro_rules! logf {
    ($($arg:tt)*) => { $crate::util::log(&format!($($arg)*)) };
}

/// Install a panic hook that logs panics (with location) to the log file.
pub fn install_panic_log() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let loc = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        let msg = info.payload().downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| info.payload().downcast_ref::<String>().cloned()).unwrap_or_default();
        log(&format!("PANIC at {}: {}", loc, msg));
        if std::env::var("AMUX_PANIC_STDERR").is_ok() {
            prev(info);
        }
    }));
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc = 0u32;
    let mut bits = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' => continue,
            _ => return None,
        } as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

pub fn home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/".into())
}

pub fn uid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// RAII terminal raw-mode guard for stdin.
pub struct RawMode {
    saved: Option<rustix::termios::Termios>,
}

impl RawMode {
    pub fn enter() -> RawMode {
        let fd = rustix::stdio::stdin();
        let saved = rustix::termios::tcgetattr(fd).ok();
        if let Some(t) = &saved {
            let mut r = t.clone();
            r.make_raw();
            let _ = rustix::termios::tcsetattr(fd, rustix::termios::OptionalActions::Now, &r);
        }
        RawMode { saved }
    }

    pub fn restore(&mut self) {
        if let Some(t) = self.saved.take() {
            let _ = rustix::termios::tcsetattr(rustix::stdio::stdin(), rustix::termios::OptionalActions::Now, &t);
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Whether two paths refer to the same file or socket.
pub fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => a == b,
    }
}

/// Known interactive shell executable names.
pub const SHELLS: &[&str] =
    &["bash", "zsh", "fish", "sh", "dash", "ksh", "mksh", "ash", "tcsh", "csh", "nu", "elvish", "xonsh", "pwsh", "yash", "osh", "ysh", "busybox"];

pub fn is_shell(name: &str) -> bool {
    SHELLS.contains(&name)
}

/// Expand a leading `~` to $HOME.
pub fn expand_home(p: &str) -> String {
    if p == "~" {
        home()
    } else if let Some(r) = p.strip_prefix("~/") {
        format!("{}/{}", home(), r)
    } else {
        p.to_string()
    }
}

pub fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./:=@%+,".contains(&b)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

pub fn hostname() -> String {
    static H: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    H.get_or_init(|| rustix::system::uname().nodename().to_string_lossy().into_owned()).clone()
}

/// Deadline helper: earliest of optional instants.
pub fn earliest(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

pub fn timeout_until(deadline: Option<Instant>) -> Option<Duration> {
    deadline.map(|d| d.saturating_duration_since(Instant::now()))
}

/// Read /proc/<pid>/cmdline as argv.
pub fn proc_cmdline(pid: u32) -> Option<Vec<String>> {
    let b = std::fs::read(format!("/proc/{}/cmdline", pid)).ok()?;
    let v: Vec<String> = b.split(|&c| c == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect();
    if v.is_empty() { None } else { Some(v) }
}

pub fn proc_cwd(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{}/cwd", pid)).ok().map(|p| p.to_string_lossy().into_owned())
}

/// Process fields read from `/proc/<pid>/stat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcStat {
    pub pgrp: u32,
    pub session: u32,
    pub tpgid: Option<u32>,
    pub start_ticks: u64,
}

pub fn proc_stat(pid: u32) -> Option<ProcStat> {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    // Fields after the command name, which is in parentheses and may contain spaces.
    let f: Vec<&str> = stat.get(stat.rfind(')')? + 2..)?.split_whitespace().collect();
    let pgrp = f.get(2)?.parse().ok()?;
    let session = f.get(3)?.parse().ok()?;
    let tpgid = f.get(5).and_then(|s| s.parse::<i64>().ok()).filter(|&g| g > 0).map(|g| g as u32);
    let start_ticks = f.get(19)?.parse().ok()?;
    Some(ProcStat { pgrp, session, tpgid, start_ticks })
}

pub fn proc_comm(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{}/comm", pid)).ok().map(|s| s.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn b64() {
        for s in ["", "f", "fo", "foo", "foob", "fooba", "foobar"] {
            assert_eq!(base64_decode(&base64_encode(s.as_bytes())).unwrap(), s.as_bytes());
        }
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
    }
    #[test]
    fn quote() {
        assert_eq!(shell_quote("abc"), "abc");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }
    #[test]
    fn proc_stat_reads_self() {
        let st = proc_stat(std::process::id()).unwrap();
        assert_eq!(st.pgrp, unsafe { libc::getpgrp() } as u32);
        assert!(st.start_ticks > 0);
        assert_eq!(proc_stat(u32::MAX - 1), None);
    }
}
