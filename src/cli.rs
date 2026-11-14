//! Command-line entry: tmux-compatible CLI, the attach client (with automatic
//! reconnect when the server restarts) and the server supervisor.

use crate::paths::Paths;
use crate::proto::{self, Conn, Dec, Enc, c2s, s2c};
use crate::{logf, pty, util};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

const USAGE: &str = "usage: amux [-2uV] [-L socket-name] [-f file] [command [flags]]

amux is a crash-proof, agent-aware terminal multiplexer that understands
tmux commands, key bindings and ~/.tmux.conf.

Common commands (tmux syntax works too):
  amux                          new session (same as `amux new`)
  amux new -s NAME | attach -t NAME | ls | kill-session -t NAME
  amux agents [--json] [--watch]       list coding agents and what they are doing
  amux agents --kinds                  agent CLIs amux recognises (plugins can add more)
  amux status set STATE [MESSAGE]      report status from inside a pane (hooks)
  amux exec [-d] [-T secs] -- CMD...   run CMD in a new window, wait, print output
  amux await -t PANE [--idle S|--pattern RE|--exit|--state S] [--timeout S]
  amux capture -t PANE                 print a pane's full scrollback
  amux events                          stream JSON events (agent state, bells, exits)
  amux plugin new NAME | list | show NAME | reload NAME | logs NAME
                                       extend amux live with hot-reloaded plugins
  amux plugin view | trace NAME        debug plugins: live logs, protocol traces
  amux plugin guide                    how to write plugins (for people and agents)
  amux migrate [--apply]               move tmux sessions into amux (tmux is not changed)

Inside amux: prefix + A opens the agent dashboard, prefix + N jumps to the
next agent that needs you, prefix + P opens the plugin view. See README.md for
the full guide.";

pub fn main(args: Vec<String>) -> i32 {
    let mut name = std::env::var("AMUX_SERVER").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "default".into());
    let mut config: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "-L" => {
                i += 1;
                name = args.get(i).cloned().unwrap_or(name);
            }
            "-S" => {
                i += 1;
                if let Some(p) = args.get(i) {
                    name = p.rsplit('/').next().unwrap_or(p).to_string();
                }
            }
            "-f" => {
                i += 1;
                config = args.get(i).map(|s| util::expand_home(s));
            }
            "-V" | "--version" => {
                println!("amux {} (tmux-compatible)", env!("CARGO_PKG_VERSION"));
                return 0;
            }
            "-h" | "--help" | "help" => {
                println!("{}", USAGE);
                return 0;
            }
            "-c" => {
                // tmux -c: run a shell command (login-shell compatibility).
                let cmd = args.get(i + 1).cloned().unwrap_or_default();
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
                let st = std::process::Command::new(shell).arg("-c").arg(cmd).status();
                return st.map(|s| s.code().unwrap_or(1)).unwrap_or(1);
            }
            "-C" | "-CC" => {
                eprintln!("amux: control mode is not supported");
                return 1;
            }
            s if s.len() > 1 && s.starts_with('-') && !s.starts_with("--") && s.chars().skip(1).all(|c| "2uvlDN".contains(c)) => {}
            _ => break,
        }
        i += 1;
    }
    let mut cmd: Vec<String> = args[i.min(args.len())..].to_vec();
    match cmd.first().map(|s| s.as_str()) {
        Some("__server") => return crate::server::main(cmd.get(1).map(|s| s.as_str()).unwrap_or(&name), cmd.get(2).cloned()),
        Some("__supervisor") => return supervisor(cmd.get(1).map(|s| s.as_str()).unwrap_or(&name), cmd.get(2).cloned()),
        Some("version") => {
            println!("amux {}", env!("CARGO_PKG_VERSION"));
            return 0;
        }
        Some("migrate") => return crate::migrate::main(&name, config, &cmd[1..]),
        _ => {}
    }
    if cmd.is_empty() {
        cmd.push("new-session".into());
    }
    cmd = translate_long_options(cmd);
    run_command(&name, config, cmd)
}

/// Map friendly long options of amux commands onto getopt flags (also used
/// for commands plugins run, so `agents --json` works there too).
pub fn translate_long_options(cmd: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(cmd.len());
    let mut after_dashdash = false;
    for a in cmd {
        if after_dashdash {
            out.push(a);
            continue;
        }
        let m = match a.as_str() {
            "--" => {
                after_dashdash = true;
                "--"
            }
            "--json" => "-j",
            "--watch" => "-w",
            "--timeout" | "--tiles" => "-T",
            "--idle" => "-i",
            "--pattern" => "-p",
            "--exit" => "-x",
            "--state" => "-s",
            "--keep" | "--kinds" => "-k",
            "--name" => "-n",
            "--cwd" => "-c",
            "--detach" => "-d",
            "--target" => "-t",
            "--lang" => "-l",
            "--force" => "-f",
            _ => {
                out.push(a);
                continue;
            }
        };
        out.push(m.to_string());
    }
    out
}

fn needs_server_start(cmd: &[String], paths: &Paths) -> bool {
    let c = crate::cmd::resolve_command(&cmd[0]).unwrap_or(cmd[0].as_str());
    match c {
        "new-session" | "start-server" | "exec" | "__migrate-import" => true,
        // A persisted state means sessions can be recovered (e.g. after a reboot).
        "attach-session" | "list-sessions" | "has-session" | "agents" => std::fs::read_to_string(paths.state_file())
            .ok()
            .and_then(|s| serde_json::from_str::<crate::model::State>(&s).ok())
            .map(|st| !st.sessions.is_empty())
            .unwrap_or(false),
        _ => false,
    }
}

fn connect(paths: &Paths) -> io::Result<UnixStream> {
    UnixStream::connect(paths.server_sock())
}

fn start_server(paths: &Paths, config: &Option<String>) -> io::Result<()> {
    paths.ensure()?;
    let mut argv = vec![crate::server::exe_path(), "__supervisor".to_string(), paths.name.clone()];
    if let Some(c) = config {
        argv.push(c.clone());
    }
    pty::spawn_daemon(&argv, None, Some(&paths.log_file()))?;
    Ok(())
}

fn connect_or_start(paths: &Paths, config: &Option<String>, start: bool) -> io::Result<UnixStream> {
    if let Ok(s) = connect(paths) {
        return Ok(s);
    }
    if !start {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("no server running on {}", paths.server_sock().display())));
    }
    start_server(paths, config)?;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(10) {
        if let Ok(s) = connect(paths) {
            return Ok(s);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err(io::Error::new(io::ErrorKind::TimedOut, "server did not start (see amux.log under ~/.local/state/amux)"))
}

fn term_size() -> (u16, u16) {
    rustix::termios::tcgetwinsize(rustix::stdio::stdout()).map(|w| (w.ws_col, w.ws_row)).ok().filter(|(c, r)| *c > 0 && *r > 0).unwrap_or((0, 0))
}

fn is_tty() -> bool {
    rustix::termios::isatty(rustix::stdio::stdin()) && rustix::termios::isatty(rustix::stdio::stdout())
}

fn hello(conn: &mut Conn) {
    let (cols, rows) = if is_tty() { term_size() } else { (0, 0) };
    let term = if is_tty() { std::env::var("TERM").unwrap_or_else(|_| "xterm".into()) } else { String::new() };
    let mut e = Enc::new();
    e.u32(proto::VERSION).u32(0).u16(cols).u16(rows).str(&term).str(&std::env::var("COLORTERM").unwrap_or_default());
    e.str(&std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default());
    e.u32(std::process::id());
    let keys = [
        "DISPLAY",
        "KRB5CCNAME",
        "SSH_ASKPASS",
        "SSH_AUTH_SOCK",
        "SSH_AGENT_PID",
        "SSH_CONNECTION",
        "WINDOWID",
        "XAUTHORITY",
        "COLORTERM",
        "TERM_PROGRAM",
        "AMUX_PANE",
        // To refuse attaching from a tmux pane that amux shows live.
        "TMUX",
        "TMUX_PANE",
        "LANG",
        "LC_ALL",
    ];
    let env: Vec<(String, String)> = keys.iter().filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v))).collect();
    e.var(env.len() as u64);
    for (k, v) in &env {
        e.str(k).str(v);
    }
    conn.send(c2s::HELLO, &e.buf);
}

fn write_all_stdout(b: &[u8]) {
    let mut out = io::stdout().lock();
    let _ = out.write_all(b);
    let _ = out.flush();
}

enum Done {
    Exit(i32, String),
    Lost,
}

/// Run an amux command (starting the server if the command needs one).
pub fn run_command(name: &str, config: Option<String>, cmd: Vec<String>) -> i32 {
    let paths = Paths::new(name);
    util::ignore_signal(libc::SIGPIPE);
    let start = needs_server_start(&cmd, &paths);
    let stream = match connect_or_start(&paths, &config, start) {
        Ok(s) => s,
        Err(e) => {
            if e.kind() == io::ErrorKind::NotFound && matches!(crate::cmd::resolve_command(&cmd[0]), Some("kill-server")) {
                return 0;
            }
            eprintln!("amux: {}", e);
            return 1;
        }
    };
    let Ok(mut conn) = Conn::new(stream) else { return 1 };
    hello(&mut conn);
    conn.send(c2s::CMD, &Enc::new().strs(&cmd).buf);
    if conn.flush_blocking().is_err() {
        eprintln!("amux: lost server");
        return 1;
    }
    let sigs = match util::Signals::install(&[libc::SIGWINCH, libc::SIGTERM, libc::SIGHUP, libc::SIGCONT, libc::SIGINT]) {
        Ok(s) => s,
        Err(_) => return 1,
    };
    let mut raw: Option<util::RawMode> = None;
    let mut session: Option<u32> = None;
    loop {
        match client_loop(&mut conn, &sigs, &mut raw, &mut session) {
            Done::Exit(code, msg) => {
                if let Some(mut r) = raw.take() {
                    r.restore();
                }
                if !msg.is_empty() {
                    println!("{}", msg);
                }
                return code;
            }
            Done::Lost => {
                if raw.is_none() {
                    eprintln!("amux: server exited unexpectedly");
                    return 1;
                }
                // Attached and the server went away: it is being restarted
                // (crash-only design). Reconnect and re-attach transparently.
                match reconnect(&paths, &config, session, &sigs) {
                    Some(c) => conn = c,
                    None => {
                        if let Some(mut r) = raw.take() {
                            r.restore();
                        }
                        write_all_stdout(&crate::server::render::terminal_reset());
                        eprintln!("amux: lost the server and could not reconnect");
                        return 1;
                    }
                }
            }
        }
    }
}

fn status_notice(msg: &str) {
    let (_, rows) = term_size();
    let s = format!("\x1b7\x1b[{};1H\x1b[0;30;43m\x1b[2K {} \x1b[0m\x1b8", rows.max(1), msg);
    write_all_stdout(s.as_bytes());
}

fn reconnect(paths: &Paths, config: &Option<String>, session: Option<u32>, sigs: &util::Signals) -> Option<Conn> {
    status_notice("amux: server restarting - your panes are safe, reconnecting...");
    let t0 = Instant::now();
    let mut started = false;
    while t0.elapsed() < Duration::from_secs(30) {
        for s in sigs.drain() {
            if s == libc::SIGTERM || s == libc::SIGHUP {
                return None;
            }
        }
        // Drain stdin so typing during the outage does not pile up.
        let mut pfd = [util::pfd(0, util::POLLIN)];
        if util::poll(&mut pfd, Some(Duration::from_millis(0))).unwrap_or(0) > 0 {
            let mut b = [0u8; 1024];
            let _ = io::stdin().read(&mut b);
        }
        match connect(paths) {
            Ok(s) => {
                let mut c = Conn::new(s).ok()?;
                hello(&mut c);
                let mut cmd = vec!["attach-session".to_string()];
                if let Some(sid) = session {
                    cmd.push("-t".into());
                    cmd.push(format!("${}", sid));
                }
                c.send(c2s::CMD, &Enc::new().strs(&cmd).buf);
                if c.flush_blocking().is_ok() {
                    return Some(c);
                }
            }
            Err(_) => {
                if !started && t0.elapsed() > Duration::from_millis(1500) {
                    // No supervisor brought it back: start one ourselves.
                    let _ = start_server(paths, config);
                    started = true;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

/// Query the outer terminal's default colours (OSC 10/11) for apps that ask.
fn query_colors(conn: &mut Conn, pending_input: &mut Vec<u8>) {
    write_all_stdout(b"\x1b]10;?\x07\x1b]11;?\x07");
    let t0 = Instant::now();
    let mut buf: Vec<u8> = Vec::new();
    let mut fg = None;
    let mut bg = None;
    while t0.elapsed() < Duration::from_millis(150) && (fg.is_none() || bg.is_none()) {
        let mut pfd = [util::pfd(0, util::POLLIN)];
        if util::poll(&mut pfd, Some(Duration::from_millis(30))).unwrap_or(0) == 0 {
            continue;
        }
        let mut b = [0u8; 512];
        match io::stdin().read(&mut b) {
            Ok(n) if n > 0 => buf.extend_from_slice(&b[..n]),
            _ => break,
        }
        while let Some(start) = buf.windows(3).position(|w| w == b"\x1b]1") {
            let rest = &buf[start..];
            let end = rest.iter().position(|&c| c == 0x07).map(|e| (e, 1)).or_else(|| rest.windows(2).position(|w| w == b"\x1b\\").map(|e| (e, 2)));
            let Some((e, tl)) = end else { break };
            let body = String::from_utf8_lossy(&rest[2..e]).into_owned();
            let parse = |v: &str| -> Option<(u8, u8, u8)> {
                let rgb = v.strip_prefix("rgb:")?;
                let p: Vec<u8> = rgb.split('/').filter_map(|h| u16::from_str_radix(&h[..h.len().min(2)], 16).ok().map(|x| x as u8)).collect();
                (p.len() == 3).then(|| (p[0], p[1], p[2]))
            };
            if let Some(v) = body.strip_prefix("10;") {
                fg = parse(v);
            } else if let Some(v) = body.strip_prefix("11;") {
                bg = parse(v);
            }
            buf.drain(start..start + e + tl);
        }
    }
    pending_input.extend_from_slice(&buf);
    if let (Some(f), Some(b)) = (fg, bg) {
        let v = format!("{},{},{};{},{},{}", f.0, f.1, f.2, b.0, b.1, b.2);
        conn.send(c2s::COLORS, &Enc::new().str(&v).buf);
    }
}

fn client_loop(conn: &mut Conn, sigs: &util::Signals, raw: &mut Option<util::RawMode>, session: &mut Option<u32>) -> Done {
    let mut inbuf = vec![0u8; 65536];
    let mut pending_input: Vec<u8> = Vec::new();
    loop {
        if !pending_input.is_empty() && raw.is_some() {
            conn.send(c2s::INPUT, &pending_input);
            pending_input.clear();
        }
        if conn.flush().is_err() {
            return Done::Lost;
        }
        let mut fds = vec![util::pfd(sigs.fd(), util::POLLIN), util::pfd(conn.fd(), util::POLLIN | if conn.pending_out() > 0 { util::POLLOUT } else { 0 })];
        if raw.is_some() {
            fds.push(util::pfd(0, util::POLLIN));
        }
        let _ = util::poll(&mut fds, None);
        if fds[0].revents != 0 {
            for s in sigs.drain() {
                match s {
                    libc::SIGWINCH => {
                        let (c, r) = term_size();
                        conn.send(c2s::RESIZE, &Enc::new().u16(c).u16(r).buf);
                    }
                    libc::SIGTERM | libc::SIGHUP => {
                        if raw.is_some() {
                            write_all_stdout(&crate::server::render::terminal_reset());
                        }
                        return Done::Exit(0, "[terminated]".into());
                    }
                    libc::SIGCONT => {
                        if let Some(r) = raw.as_mut() {
                            *r = util::RawMode::enter();
                            write_all_stdout(b"\x1b[?1049h");
                            let (c, rr) = term_size();
                            conn.send(c2s::RESIZE, &Enc::new().u16(c).u16(rr).buf);
                        }
                    }
                    libc::SIGINT => {
                        if raw.is_none() {
                            return Done::Exit(130, String::new());
                        }
                    }
                    _ => {}
                }
            }
        }
        if fds.len() > 2 && fds[2].revents & (util::POLLIN | util::POLLERR) != 0 {
            match io::stdin().read(&mut inbuf) {
                Ok(0) => return Done::Exit(0, String::new()),
                Ok(n) => conn.send(c2s::INPUT, &inbuf[..n]),
                Err(_) => {}
            }
        }
        if fds[1].revents & (util::POLLIN | util::POLLERR) != 0 {
            let _ = conn.fill();
            loop {
                match conn.next() {
                    Ok(Some((ty, p))) => match ty {
                        s2c::OUTPUT | s2c::STDOUT => write_all_stdout(&p),
                        s2c::STDERR => {
                            let _ = io::stderr().write_all(&p);
                        }
                        s2c::ATTACHED => {
                            let mut d = Dec::new(&p);
                            *session = d.u32().ok();
                            if raw.is_none() {
                                *raw = Some(util::RawMode::enter());
                                query_colors(conn, &mut pending_input);
                            }
                        }
                        s2c::SUSPEND => {
                            if let Some(r) = raw.as_mut() {
                                r.restore();
                            }
                            // SAFETY: stop ourselves; SIGCONT resumes and re-enters raw mode.
                            unsafe {
                                libc::signal(libc::SIGTSTP, libc::SIG_DFL);
                                libc::raise(libc::SIGTSTP);
                            }
                        }
                        s2c::EXIT => {
                            let mut d = Dec::new(&p);
                            let code = d.i32().unwrap_or(0);
                            let msg = d.string().unwrap_or_default();
                            return Done::Exit(code, msg);
                        }
                        _ => {}
                    },
                    Ok(None) => break,
                    Err(_) => return Done::Lost,
                }
            }
            if conn.eof {
                return Done::Lost;
            }
        }
    }
}

/// Supervisor: restarts the server if it crashes or hangs.
fn supervisor(name: &str, config: Option<String>) -> i32 {
    let paths = Paths::new(name);
    let _ = paths.ensure();
    util::log_init(&paths.log_file(), "supervisor");
    let lock = paths.run.join("supervisor.lock");
    let Ok(f) = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&lock) else { return 1 };
    let lfd: OwnedFd = f.into();
    // A previous supervisor may still be waiting for its server to finish
    // exiting (kill-server): take over once it is gone.
    let t0 = Instant::now();
    // SAFETY: flock on our own descriptor.
    while unsafe { libc::flock(lfd.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        if t0.elapsed() > Duration::from_secs(3) {
            return 0;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    util::ignore_signal(libc::SIGHUP);
    util::ignore_signal(libc::SIGPIPE);
    let mut restarts = 0u32;
    let mut recent: Vec<Instant> = Vec::new();
    loop {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: local array for pipe2.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return 1;
        }
        // SAFETY: fresh descriptors.
        let (rd, wr) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let mut cmd = std::process::Command::new(crate::server::exe_path());
        cmd.arg0("amux");
        cmd.arg("__server").arg(name);
        if let Some(c) = &config {
            cmd.arg(c);
        }
        cmd.env("AMUX_RESTARTS", restarts.to_string());
        cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null());
        let wfd = wr.as_raw_fd();
        use std::os::unix::process::CommandExt;
        // SAFETY: dup2 only, between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                if libc::dup2(wfd, 4) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let started = Instant::now();
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                logf!("cannot start server: {}", e);
                return 1;
            }
        };
        drop(wr);
        let _ = util::set_nonblocking(rd.as_raw_fd(), true);
        let mut last_beat = Instant::now();
        let status = loop {
            let mut pfd = [util::pfd(rd.as_raw_fd(), util::POLLIN)];
            let _ = util::poll(&mut pfd, Some(Duration::from_secs(5)));
            let mut b = [0u8; 64];
            match rustix::io::read(&rd, &mut b) {
                Ok(0) => break child.wait().ok(),
                Ok(_) => last_beat = Instant::now(),
                Err(_) => {}
            }
            if let Ok(Some(st)) = child.try_wait() {
                break Some(st);
            }
            if last_beat.elapsed() > Duration::from_secs(30) {
                logf!("server unresponsive for 30s; killing it");
                let _ = child.kill();
                break child.wait().ok();
            }
        };
        let code = status.and_then(|s| s.code());
        match code {
            Some(0 | 75) => {
                logf!("server exited cleanly ({:?})", code);
                return 0;
            }
            _ => {
                restarts += 1;
                logf!("server died ({:?}); restart #{}", status, restarts);
                recent.retain(|t| t.elapsed() < Duration::from_secs(60));
                recent.push(Instant::now());
                if started.elapsed() < Duration::from_secs(2) {
                    std::thread::sleep(Duration::from_millis((100u64 << recent.len().min(6)).min(5000)));
                }
                if recent.len() > 20 {
                    logf!("server crash loop; giving up");
                    return 1;
                }
            }
        }
    }
}
