//! Keeper process: frames, reconnect after a "server crash", panic recovery.

use amux::keeper::{Spec, decode_frame};
use amux::proto::{Conn, Dec, Enc, k2s, s2k};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

fn bin() -> String {
    env!("CARGO_BIN_EXE_amux").to_string()
}

struct Screen {
    rows: Vec<String>,
}

fn start(dir: &std::path::Path, argv: &[&str]) -> (std::process::Child, String) {
    let sock = dir.join("k1.sock").to_string_lossy().into_owned();
    let spec = Spec {
        id: 1,
        sock: sock.clone(),
        argv: argv.iter().map(|s| s.to_string()).collect(),
        cwd: dir.to_string_lossy().into_owned(),
        env: vec![("TERM".into(), "xterm-256color".into()), ("PATH".into(), std::env::var("PATH").unwrap()), ("PS1".into(), "$ ".into())],
        cols: 40,
        rows: 6,
        history: 1000,
        log: dir.join("k.log").to_string_lossy().into_owned(),
        ..Default::default()
    };
    let child = std::process::Command::new(bin()).arg("__keeper").arg(serde_json::to_string(&spec).unwrap()).spawn().unwrap();
    (child, sock)
}

fn connect(sock: &str) -> Conn {
    let t0 = Instant::now();
    loop {
        if let Ok(s) = UnixStream::connect(sock) {
            let mut c = Conn::new(s).unwrap();
            c.send(s2k::HELLO, &Enc::new().u32(1).buf);
            c.flush_blocking().unwrap();
            return c;
        }
        assert!(t0.elapsed() < Duration::from_secs(5), "keeper socket never appeared");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Pump frames into a local screen until `pred` holds.
fn wait_screen(c: &mut Conn, scr: &mut Screen, pred: impl Fn(&Screen) -> bool) -> bool {
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(5) {
        let _ = c.fill();
        while let Ok(Some((ty, p))) = c.next() {
            if ty == k2s::FRAME {
                let f = decode_frame(&p).unwrap();
                if scr.rows.len() != f.rows || f.full {
                    scr.rows = vec![String::new(); f.rows];
                }
                for (y, _w, cells) in f.lines {
                    let s: String = cells.iter().map(|c| char::from_u32(c.ch).filter(|&ch| ch != '\0').unwrap_or(' ')).collect();
                    scr.rows[y] = s.trim_end().to_string();
                }
                c.send(s2k::ACK, &[]);
                c.flush().unwrap();
            }
        }
        if pred(scr) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn keeper_survives_server_disconnect_and_recovers_from_panic() {
    let dir = std::env::temp_dir().join(format!("amux-keeper-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (mut child, sock) = start(&dir, &["/bin/sh"]);
    let mut c = connect(&sock);
    let mut scr = Screen { rows: vec![] };
    c.send(s2k::INPUT, b"echo hello-$((40+2))\r");
    c.flush_blocking().unwrap();
    assert!(wait_screen(&mut c, &mut scr, |s| s.rows.iter().any(|r| r.contains("hello-42"))), "{:?}", scr.rows);

    // "Server crash": drop the connection, keep typing via a new one.
    drop(c);
    std::thread::sleep(Duration::from_millis(100));
    let mut c = connect(&sock);
    let mut scr2 = Screen { rows: vec![] };
    assert!(wait_screen(&mut c, &mut scr2, |s| s.rows.iter().any(|r| r.contains("hello-42"))), "full frame after reconnect: {:?}", scr2.rows);
    c.send(s2k::INPUT, b"echo still-$((1+1))\r");
    c.flush_blocking().unwrap();
    assert!(wait_screen(&mut c, &mut scr2, |s| s.rows.iter().any(|r| r.contains("still-2"))));

    // Injected emulator panic: screen content survives, shell keeps working.
    c.send(s2k::PANIC_TEST, &[]);
    c.send(s2k::INPUT, b"echo after-panic\r");
    c.flush_blocking().unwrap();
    assert!(wait_screen(&mut c, &mut scr2, |s| s.rows.iter().any(|r| r.contains("after-panic"))), "{:?}", scr2.rows);

    // Capture with history.
    c.send(s2k::CAPTURE, &Enc::new().u32(7).i32(-100).i32(100).u32(0).buf);
    c.flush_blocking().unwrap();
    let t0 = Instant::now();
    let mut captured = None;
    while captured.is_none() && t0.elapsed() < Duration::from_secs(3) {
        let _ = c.fill();
        while let Ok(Some((ty, p))) = c.next() {
            if ty == k2s::CAPTURED {
                let mut d = Dec::new(&p);
                assert_eq!(d.u32().unwrap(), 7);
                captured = Some(d.string().unwrap());
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let text = captured.expect("capture reply");
    assert!(text.contains("hello-42") && text.contains("after-panic"), "{text}");

    // Exit: the keeper reports the child's status.
    c.send(s2k::INPUT, b"exit 3\r");
    c.flush_blocking().unwrap();
    let t0 = Instant::now();
    let mut status = None;
    while status.is_none() && t0.elapsed() < Duration::from_secs(5) {
        let _ = c.fill();
        while let Ok(Some((ty, p))) = c.next() {
            if ty == k2s::EXITED {
                status = Some(Dec::new(&p).i32().unwrap());
            }
            if ty == k2s::FRAME {
                c.send(s2k::ACK, &[]);
            }
        }
        let _ = c.flush();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(status.map(amux::pty::exit_code), Some(3));
    c.send(s2k::CLOSE, &[]);
    c.flush_blocking().unwrap();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
