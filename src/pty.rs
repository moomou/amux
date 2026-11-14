//! Pseudo-terminal creation and child spawning.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

/// Open a PTY pair sized `cols`x`rows`. Returns (master, slave).
pub fn open(cols: u16, rows: u16) -> io::Result<(OwnedFd, OwnedFd)> {
    use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC)?;
    grantpt(&master)?;
    unlockpt(&master)?;
    let name = ptsname(&master, Vec::new())?;
    let slave =
        rustix::fs::open(name.as_c_str(), rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOCTTY | rustix::fs::OFlags::CLOEXEC, rustix::fs::Mode::empty())?;
    set_size(&master, cols, rows)?;
    Ok((master, slave))
}

pub fn set_size(master: &OwnedFd, cols: u16, rows: u16) -> io::Result<()> {
    let ws = rustix::termios::Winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    rustix::termios::tcsetwinsize(master, ws)?;
    Ok(())
}

/// Foreground process group of the terminal.
pub fn fg_pgrp(master: &OwnedFd) -> Option<u32> {
    rustix::termios::tcgetpgrp(master).ok().map(|p| p.as_raw_nonzero().get() as u32)
}

pub struct SpawnSpec<'a> {
    pub argv: &'a [String],
    pub arg0: Option<&'a str>,
    pub cwd: &'a str,
    pub env: &'a [(String, String)],
}

/// Spawn a child as session leader with `slave` as its controlling terminal.
pub fn spawn(slave: &OwnedFd, spec: &SpawnSpec) -> io::Result<u32> {
    if spec.argv.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty command"));
    }
    let mut cmd = Command::new(&spec.argv[0]);
    cmd.args(&spec.argv[1..]);
    if let Some(a0) = spec.arg0 {
        cmd.arg0(a0);
    }
    let cwd = if std::path::Path::new(spec.cwd).is_dir() { spec.cwd.to_string() } else { crate::util::home() };
    cmd.current_dir(cwd);
    cmd.env_clear();
    for (k, v) in spec.env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::from(slave.try_clone()?));
    cmd.stdout(Stdio::from(slave.try_clone()?));
    cmd.stderr(Stdio::from(slave.try_clone()?));
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
            for s in [libc::SIGCHLD, libc::SIGPIPE, libc::SIGHUP, libc::SIGTERM, libc::SIGINT, libc::SIGQUIT, libc::SIGWINCH] {
                libc::signal(s, libc::SIG_DFL);
            }
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    Ok(child.id())
}

/// Spawn a detached helper process (new session, stdio to /dev/null),
/// optionally passing `pass_fd` as fd 3.
pub fn spawn_daemon(argv: &[String], pass_fd: Option<&OwnedFd>, log_stderr: Option<&std::path::Path>) -> io::Result<u32> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.arg0("amux");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    match log_stderr.and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).ok()) {
        Some(f) => cmd.stderr(Stdio::from(f)),
        None => cmd.stderr(Stdio::null()),
    };
    let fd = pass_fd.map(|f| f.as_raw_fd());
    // SAFETY: async-signal-safe calls only.
    unsafe {
        cmd.pre_exec(move || {
            libc::setsid();
            if let Some(fd) = fd
                && libc::dup2(fd, 3) < 0
            {
                return Err(io::Error::last_os_error());
            }
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    Ok(child.id())
}

/// Reap any exited children; returns (pid, raw wait status).
pub fn reap() -> Vec<(u32, i32)> {
    let mut out = Vec::new();
    loop {
        let mut status = 0;
        // SAFETY: non-blocking waitpid on any child.
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid <= 0 {
            break;
        }
        out.push((pid as u32, status));
    }
    out
}

/// Human exit code from a raw wait status (128+signal for signals).
pub fn exit_code(status: i32) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        -1
    }
}
