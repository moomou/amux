//! Differential test: feed identical random byte streams to real tmux and to
//! amux's emulator, then compare visible screen, cursor and history.
//! Skips when tmux is not installed. AMUX_ORACLE_CASES controls case count.

use amux::history;
use amux::term::Emulator;
use std::process::Command;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

fn text(r: &mut Rng, len: usize, out: &mut Vec<u8>) {
    for _ in 0..len {
        let k = r.below(100);
        let s: &str = match k {
            0..=3 => "漢",
            4 => "字",
            5 => "e\u{301}",
            6..=15 => " ",
            _ => {
                let c = (b'a' + r.below(26) as u8) as char;
                out.extend_from_slice(c.to_string().as_bytes());
                continue;
            }
        };
        out.extend_from_slice(s.as_bytes());
    }
}

fn generate(seed: u64, cols: usize, rows: usize) -> Vec<u8> {
    let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut out = Vec::new();
    let n = 30 + r.below(200);
    let maxk: u64 = std::env::var("AMUX_ORACLE_MAXK").ok().and_then(|s| s.parse().ok()).unwrap_or(100);
    let only: Option<u64> = std::env::var("AMUX_ORACLE_ONLY").ok().and_then(|s| s.parse().ok());
    for _ in 0..n {
        // Keep each stream within one PTY read so tmux parses it in one pass
        // (its ASCII-run cleanup happens per read).
        if out.len() > 3600 {
            break;
        }
        let mut k = r.below(100);
        if k >= maxk && k < 86 {
            k = 0;
        }
        if let Some(o) = only
            && (30..86).contains(&k)
            && r.below(3) != 0
        {
            k = o;
        }
        let csi = |s: String, out: &mut Vec<u8>| {
            out.extend_from_slice(b"\x1b[");
            out.extend_from_slice(s.as_bytes());
        };
        match k {
            0..=29 => {
                let len = 1 + r.below(40) as usize;
                text(&mut r, len, &mut out)
            }
            30..=39 => out.extend_from_slice(b"\r\n"),
            40..=42 => out.push(b'\n'),
            43..=44 => out.push(b'\r'),
            45..=46 => out.push(b'\t'),
            47 => out.push(8),
            48..=53 => {
                let y = 1 + r.below(rows as u64 + 2);
                let x = 1 + r.below(cols as u64 + 3);
                csi(format!("{};{}H", y, x), &mut out)
            }
            54..=56 => {
                let n = r.below(6);
                let d = ['A', 'B', 'C', 'D'][r.below(4) as usize];
                csi(format!("{}{}", n, d), &mut out)
            }
            57..=59 => csi(format!("{}K", r.below(3)), &mut out),
            60..=61 => csi(format!("{}J", r.below(2)), &mut out),
            62 => csi("2J".into(), &mut out),
            63..=64 => {
                let n = 1 + r.below(3);
                let d = ['L', 'M'][r.below(2) as usize];
                csi(format!("{}{}", n, d), &mut out)
            }
            65..=67 => {
                let n = 1 + r.below(5);
                let d = ['@', 'P', 'X'][r.below(3) as usize];
                csi(format!("{}{}", n, d), &mut out)
            }
            68..=69 => {
                if r.below(3) == 0 {
                    csi("r".into(), &mut out)
                } else {
                    let t = 1 + r.below(rows as u64 / 2);
                    let b = t + 1 + r.below(rows as u64 - t);
                    csi(format!("{};{}r", t, b), &mut out)
                }
            }
            70 => {
                let n = 1 + r.below(3);
                let d = ['S', 'T'][r.below(2) as usize];
                csi(format!("{}{}", n, d), &mut out)
            }
            71 => out.extend_from_slice(b"\x1bM"),
            72 => out.extend_from_slice(if r.below(2) == 0 { b"\x1bD" } else { b"\x1bE" }),
            73 => out.extend_from_slice(if r.below(2) == 0 { b"\x1b7" } else { b"\x1b8" }),
            74..=78 => {
                let p = ["0", "1", "31", "42", "7", "38;5;100", "48;2;1;2;3", "22;39;49", "4"][r.below(9) as usize];
                csi(format!("{}m", p), &mut out)
            }
            79 => csi(if r.below(2) == 0 { "4h".into() } else { "4l".into() }, &mut out),
            80 => csi(if r.below(4) == 0 { "?7l".into() } else { "?7h".into() }, &mut out),
            81 => csi(if r.below(3) == 0 { "?6h".into() } else { "?6l".into() }, &mut out),
            82 => csi(if r.below(2) == 0 { "?1049h".into() } else { "?1049l".into() }, &mut out),
            83 => {
                let n = 1 + r.below(cols as u64 + 2);
                let d = ['G', 'd'][r.below(2) as usize];
                csi(format!("{}{}", n, d), &mut out)
            }
            84 => csi(format!("{}b", 1 + r.below(5)), &mut out),
            85 => match r.below(4) {
                0 => out.extend_from_slice(b"\x1bH"),
                1 => csi("0g".into(), &mut out),
                2 => csi("3g".into(), &mut out),
                _ => csi(format!("{}Z", 1 + r.below(2)), &mut out),
            },
            _ => {
                let len = cols + r.below(cols as u64 * 2) as usize;
                text(&mut r, len, &mut out)
            }
        }
    }
    // Leave modes that would make the comparison depend on display state alone.
    out
}

fn tmux_ok() -> bool {
    Command::new("tmux").arg("-V").output().map(|o| o.status.success()).unwrap_or(false)
}

#[test]
fn oracle_tmux() {
    if !tmux_ok() {
        eprintln!("tmux not found; skipping");
        return;
    }
    let cases: u64 = std::env::var("AMUX_ORACLE_CASES").ok().and_then(|s| s.parse().ok()).unwrap_or(40);
    let start: u64 = std::env::var("AMUX_ORACLE_START").ok().and_then(|s| s.parse().ok()).unwrap_or(1);
    let dir = std::env::temp_dir().join(format!("amux-oracle-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // A socket inside the case directory: never another tmux server's.
    let sock = dir.join("tmux.sock").to_string_lossy().into_owned();
    let conf = dir.join("tmux.conf");
    std::fs::write(&conf, "set -g status off\nset -g history-limit 5000\nset -g default-shell /bin/sh\n").unwrap();
    let tmux = |args: &[&str]| -> String {
        let o = Command::new("tmux").args(["-S", &sock, "-f", conf.to_str().unwrap()]).args(args).env_remove("TMUX").output().unwrap();
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    let mut failures = Vec::new();
    for seed in start..start + cases {
        let (cols, rows) = (20 + (seed as usize * 7) % 30, 5 + (seed as usize * 3) % 10);
        let data = generate(seed, cols, rows);
        let f = dir.join(format!("case{}.bin", seed));
        std::fs::write(&f, &data).unwrap();
        let resize = std::env::var("AMUX_ORACLE_RESIZE").is_ok();
        let (ncols, nrows) = if resize {
            let mut rr = Rng(seed.wrapping_mul(31) | 1);
            (8 + rr.below(60) as usize, 3 + rr.below(20) as usize)
        } else {
            (cols, rows)
        };
        let run_tmux = |attempt: u32| -> (String, String, String) {
            let name = format!("c{}-{}", seed, attempt);
            let chan = format!("d{}-{}", seed, attempt);
            let cmd = format!("stty -opost; cat {}; sleep 0.15; tmux -S {} wait-for -S {}; sleep 100000", f.display(), sock, chan);
            tmux(&["new-session", "-d", "-s", &name, "-x", &cols.to_string(), "-y", &rows.to_string(), &cmd]);
            tmux(&["wait-for", &chan]);
            if resize {
                tmux(&["set", "-t", &name, "window-size", "manual"]);
                tmux(&["resize-window", "-t", &name, "-x", &ncols.to_string(), "-y", &nrows.to_string()]);
                std::thread::sleep(std::time::Duration::from_millis(30));
            }
            let t_screen = tmux(&["capture-pane", "-p", "-t", &name]);
            let t_cursor = tmux(&["display", "-p", "-t", &name, "#{cursor_x} #{cursor_y}"]);
            let hsize: usize = tmux(&["display", "-p", "-t", &name, "#{history_size}"]).trim().parse().unwrap_or(0);
            let t_hist = if hsize > 0 { tmux(&["capture-pane", "-p", "-J", "-S", "-", "-E", "-1", "-t", &name]) } else { String::new() };
            tmux(&["kill-session", "-t", &name]);
            (t_screen, t_cursor, t_hist)
        };
        let mut e = Emulator::new(cols, rows, 5000);
        e.feed(&data);
        if resize {
            e.term.resize(ncols, nrows);
        }
        let (cols, rows) = (ncols, nrows);
        let a_lines = e.term.screen_text();
        let (cx, cy) = e.term.cursor();
        let cx = if e.term.cursor_pending() { cols } else { cx };
        let a_cursor = format!("{} {}", cx, cy);
        let a_hist: Vec<String> = e.term.hist.iter().map(|l| history::line_text(l).trim_end().to_string()).collect();

        let compare = |(t_screen, t_cursor, t_hist): (String, String, String)| -> Vec<String> {
            let mut t_lines: Vec<String> = t_screen.lines().map(|s| s.trim_end().to_string()).collect();
            t_lines.resize(rows, String::new());
            let a_lines: Vec<String> = a_lines.iter().map(|s| s.trim_end().to_string()).collect();
            let t_hist: Vec<String> = t_hist.lines().map(|s| s.trim_end().to_string()).collect();
            // tmux can leave the cursor past the last column; amux reports a pending wrap.
            let t_cursor = {
                let mut it = t_cursor.split_whitespace();
                let x: usize = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                let y = it.next().unwrap_or("0").to_string();
                format!("{} {}", x.min(cols), y)
            };
            let mut problems = Vec::new();
            if t_lines != a_lines {
                let mut d = String::new();
                for (i, (t, a)) in t_lines.iter().zip(a_lines.iter()).enumerate() {
                    if t != a {
                        d.push_str(&format!("\n   row {:2} tmux |{}|\n          amux |{}|", i, t, a));
                    }
                }
                problems.push(format!("screen differs:{}", d));
            }
            if t_cursor != a_cursor {
                problems.push(format!("cursor tmux {} amux {}", t_cursor, a_cursor));
            }
            let squash = |v: &Vec<String>| -> Vec<String> { v.iter().map(|l| l.replace(' ', "")).collect() };
            // After a reflow tmux drops wide-char padding spaces from history joins;
            // amux keeps history width-independent, so compare without spaces.
            let hist_differs = if resize { squash(&t_hist) != squash(&a_hist) } else { t_hist != a_hist };
            if hist_differs {
                problems.push(format!("history differs: tmux {} lines, amux {} lines", t_hist.len(), a_hist.len()));
            }
            problems
        };
        let mut problems = compare(run_tmux(0));
        if !problems.is_empty() {
            // tmux can capture before it has drained the pane under load; retry once.
            problems = compare(run_tmux(1));
        }
        if !problems.is_empty() {
            failures.push(format!("seed {} ({}x{}): {}", seed, cols, rows, problems.join("; ")));
        }
    }
    tmux(&["kill-server"]);
    if std::env::var("AMUX_ORACLE_KEEP").is_err() {
        let _ = std::fs::remove_dir_all(&dir);
    } else {
        eprintln!("cases kept in {}", dir.display());
    }
    if !failures.is_empty() {
        for f in &failures {
            eprintln!("{}", f);
        }
        panic!("{} of {} cases differ from tmux", failures.len(), cases);
    }
}
