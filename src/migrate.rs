//! `amux migrate`: move tmux sessions into amux without touching tmux.
//!
//! Without `--apply` this only prints the plan. With it, tmux is read (never
//! changed), each pane's history is saved for the new amux pane, and the
//! amux server builds the sessions (`__migrate-import`, src/server/migrate.rs).

use crate::paths::Paths;
use crate::tmux::{self, Mode, Plan, PlanPane, PlanSession, PlanWindow, TPane, Tmux};

const USAGE: &str = "usage: amux migrate [-S socket | -L name] [-t session]... [--fresh | --live] [--apply]

Moves tmux sessions into amux. tmux itself is never changed: amux only reads
it, and programs keep running there.
  live panes   (agents, editors, servers, ssh) keep running in tmux; amux
               shows and controls them, and turns them into a normal amux
               shell once they end
  fresh panes  (shells at a prompt) become new amux shells in the same
               directory, with the pane's history above them

  -S PATH / -L NAME  the tmux server (default: the one you are in, else tmux's default)
  -t SESSION         only this session (repeatable)
  --fresh            start every pane fresh (agents are resumed when amux knows how)
  --live             keep every pane live, shells too
  --apply            do it (without it, only the plan is printed)";

/// History lines carried over for fresh panes.
const HISTORY_LINES: usize = 5000;

pub fn main(name: &str, config: Option<String>, args: &[String]) -> i32 {
    match run(name, config, args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("amux migrate: {}", e);
            1
        }
    }
}

fn run(name: &str, config: Option<String>, args: &[String]) -> Result<i32, String> {
    let (mut socket, mut server, mut only, mut apply, mut force) = (None, None, Vec::new(), false, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "tmux" if socket.is_none() && server.is_none() => {}
            "-S" => socket = Some(it.next().ok_or("-S needs a path")?.clone()),
            "-L" => server = Some(it.next().ok_or("-L needs a name")?.clone()),
            "-t" => only.push(it.next().ok_or("-t needs a session")?.trim_start_matches('=').to_string()),
            "--apply" | "-y" => apply = true,
            "--fresh" => force = Some(Mode::Fresh),
            "--live" => force = Some(Mode::Live),
            "-h" | "--help" | "help" => {
                println!("{}", USAGE);
                return Ok(0);
            }
            other => return Err(format!("unknown option {}\n{}", other, USAGE)),
        }
    }
    let t = Tmux::locate(socket.as_deref(), server.as_deref());
    let mut panes = t.panes().map_err(|e| format!("no tmux server at {} ({})", t.socket.display(), e))?;
    if !only.is_empty() {
        for s in &only {
            if !panes.iter().any(|p| &p.session == s) {
                return Err(format!("tmux has no session named {}", s));
            }
        }
        panes.retain(|p| only.contains(&p.session));
    }
    if panes.is_empty() {
        println!("tmux server {} has no sessions to migrate", t.socket.display());
        return Ok(0);
    }
    let mut sessions: Vec<(String, String)> = panes.iter().map(|p| (p.session.clone(), p.session_id.clone())).collect();
    sessions.dedup();
    let mut pf = tmux::preflight(&t, &sessions);
    let plan = build_plan(&t, &panes, &mut pf, force);
    print_plan(&t, &plan, &panes, &pf);
    if !apply {
        println!("\nNothing has changed. To migrate, run the same command with --apply.");
        return Ok(0);
    }
    let paths = Paths::new(name);
    let dir = paths.run.join("migrate").join(format!("{}-{}", crate::util::now_unix(), std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {}", dir.display(), e))?;
    let mut plan = plan;
    for p in plan.sessions.iter_mut().flat_map(|s| s.windows.iter_mut()).flat_map(|w| w.panes.iter_mut()) {
        if p.mode == Mode::Live {
            continue;
        }
        // Their history, replayed in the new pane before its shell starts.
        match t.capture(&p.id, HISTORY_LINES) {
            Ok(text) => {
                let f = dir.join(format!("{}.feed", p.id.trim_start_matches('%')));
                if std::fs::write(&f, history_feed(&text)).is_ok() {
                    p.feed = Some(f.to_string_lossy().into_owned());
                }
            }
            Err(e) => eprintln!("amux migrate: no history for {}: {}", p.id, e),
        }
    }
    let file = dir.join("plan.json");
    std::fs::write(&file, serde_json::to_vec(&plan).map_err(|e| e.to_string())?).map_err(|e| format!("{}: {}", file.display(), e))?;
    println!();
    let code = crate::cli::run_command(name, config, vec!["__migrate-import".into(), file.to_string_lossy().into_owned()]);
    let _ = std::fs::remove_dir_all(&dir);
    if code == 0 {
        println!("\ntmux was not changed. Close its sessions yourself once you no longer need them.");
    }
    Ok(code)
}

fn build_plan(t: &Tmux, panes: &[TPane], pf: &mut tmux::Preflight, force: Option<Mode>) -> Plan {
    let mut plan = Plan { socket: t.socket.to_string_lossy().into_owned(), sessions: Vec::new() };
    for p in panes {
        if plan.sessions.last().is_none_or(|s| s.tmux_name != p.session) {
            plan.sessions.push(PlanSession { name: p.session.clone(), tmux_name: p.session.clone(), windows: Vec::new() });
        }
        let Some(s) = plan.sessions.last_mut() else { continue };
        if s.windows.last().is_none_or(|w| w.index != p.window_index) {
            s.windows.push(PlanWindow {
                index: p.window_index,
                name: p.window_name.clone(),
                auto_name: p.auto_name,
                active: p.window_active,
                layout: p.layout.clone(),
                width: p.width,
                height: p.height,
                zoomed: p.zoomed,
                panes: Vec::new(),
            });
        }
        let auto = tmux::classify(p);
        let mut mode = match (auto, force) {
            (Mode::Dead, _) => Mode::Dead,
            (_, Some(f)) => f,
            (m, None) => m,
        };
        if mode == Mode::Live && (!pf.live_ok || pf.no_live.contains(&p.session)) {
            mode = Mode::Fresh;
        }
        if mode == Mode::Live && runs_amux(t, p) {
            mode = Mode::Fresh;
            pf.notes.push(format!("{} runs amux: it starts fresh (live, amux would show itself inside itself)", p.pane_id));
        }
        let pane = PlanPane {
            id: p.pane_id.clone(),
            mode,
            active: p.active,
            cwd: p.cwd.clone(),
            command: p.command.clone(),
            fg_cmd: tmux::foreground_cmdline(p.pid),
            pid: p.pid,
            start_ticks: crate::util::proc_stat(p.pid).map_or(0, |s| s.start_ticks),
            feed: None,
        };
        if let Some(w) = s.windows.last_mut() {
            w.panes.push(pane);
        }
    }
    plan
}

/// The pane `amux migrate` runs in, or one running another amux command
/// (an attached client, say).
fn runs_amux(t: &Tmux, p: &TPane) -> bool {
    let here = std::env::var("TMUX_PANE").is_ok_and(|id| id == p.pane_id)
        && std::env::var("TMUX").ok().and_then(|v| v.split(',').next().map(std::path::PathBuf::from)).is_some_and(|s| crate::util::same_file(&s, &t.socket));
    here || tmux::foreground(p.pid).is_some_and(tmux::is_amux)
}

fn print_plan(t: &Tmux, plan: &Plan, panes: &[TPane], pf: &tmux::Preflight) {
    let windows: usize = plan.sessions.iter().map(|s| s.windows.len()).sum();
    println!("tmux server {} · {} session(s) · {} window(s) · {} pane(s)", t.socket.display(), plan.sessions.len(), windows, panes.len());
    println!("tmux is not changed: amux only reads it, and live panes keep running there.");
    let home = crate::util::home();
    let short = |p: &str| if !home.is_empty() && p.starts_with(&home) { format!("~{}", &p[home.len()..]) } else { p.to_string() };
    for s in &plan.sessions {
        let attached = panes.iter().find(|p| p.session == s.tmux_name).map_or(0, |p| p.attached);
        println!("\n{}{} → amux session {}", s.tmux_name, if attached > 0 { " (attached in tmux)" } else { "" }, s.name);
        for w in &s.windows {
            println!("  window {} {:<16} {}x{}, {} pane(s){}", w.index, w.name, w.width, w.height, w.panes.len(), if w.zoomed { ", zoomed" } else { "" });
            for p in &w.panes {
                let what = match p.mode {
                    Mode::Live => "keeps running in tmux; live in amux".to_string(),
                    Mode::Fresh => "new shell here, below its tmux history".to_string(),
                    Mode::Dead => "had exited; its history only".to_string(),
                };
                let mode = match p.mode {
                    Mode::Live => "live",
                    Mode::Fresh => "fresh",
                    Mode::Dead => "dead",
                };
                println!("    {:<5} {:<5} {:<14} {:<28} {}", p.id, mode, p.command, short(&p.cwd), what);
            }
        }
    }
    if !pf.notes.is_empty() {
        println!("\nNotes:");
        for n in &pf.notes {
            println!("  - {}", n);
        }
    }
}

/// A pane's captured text as terminal output: the lines, then attributes reset.
fn history_feed(text: &str) -> Vec<u8> {
    let lines: Vec<&str> = text.trim_end_matches(['\n', ' ']).lines().collect();
    let mut out = Vec::with_capacity(text.len() + lines.len() + 16);
    for l in lines {
        out.extend_from_slice(l.as_bytes());
        out.extend_from_slice(b"\x1b[0m\r\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_feeds_end_lines_with_crlf_and_reset() {
        assert_eq!(history_feed("a\n\x1b[31mb\n\n\n"), b"a\x1b[0m\r\n\x1b[31mb\x1b[0m\r\n".to_vec());
        assert!(history_feed("").is_empty());
    }
}
