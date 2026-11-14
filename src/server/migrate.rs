//! Carrying out a migration from tmux (`amux migrate --apply`): sessions and
//! windows with the same names and layouts. Live panes show their tmux pane
//! through `amux __tmux-bridge` (the program keeps running in tmux); fresh
//! panes start a shell in the same directory, after the pane's history.

use super::exec::Flow;
use super::{Server, Start};
use crate::model::{PaneMeta, Rect, Session, Window};
use crate::tmux::{self, Mode, Plan, PlanSession, PlanWindow};
use crate::{format, logf, util};
use std::collections::HashMap;
use std::path::Path;

type R = Result<Flow, String>;

impl Server {
    /// `__migrate-import PLANFILE` (run by `amux migrate --apply`).
    pub fn migrate_import(&mut self, cid: u32, rest: &[String]) -> R {
        let path = rest.first().ok_or("usage: __migrate-import PLANFILE")?;
        let dir = self.paths.run.join("migrate");
        // Files named in a plan are deleted once replayed: only accept ours.
        let ours = |p: &str| Path::new(p).starts_with(&dir) && !p.contains("..");
        if !ours(path) {
            return Err(format!("migration plans are read from {}", dir.display()));
        }
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {}", path, e))?;
        let _ = std::fs::remove_file(path);
        let mut plan: Plan = serde_json::from_str(&text).map_err(|e| format!("bad migration plan: {}", e))?;
        for p in plan.sessions.iter_mut().flat_map(|s| s.windows.iter_mut()).flat_map(|w| w.panes.iter_mut()) {
            if p.feed.as_deref().is_some_and(|f| !ours(f)) {
                p.feed = None;
            }
        }
        let mut lines = Vec::new();
        for ps in &plan.sessions {
            match self.import_session(&plan.socket, ps) {
                Ok(l) => lines.push(l),
                Err(e) => lines.push(format!("{}: not migrated: {}", ps.tmux_name, e)),
            }
        }
        self.mark_dirty();
        self.redraw_all();
        self.output(cid, "migrate", lines);
        Ok(Flow::Next)
    }

    /// Why a client must not attach, if its terminal is a tmux pane that amux
    /// shows live: amux would draw itself inside itself, and every key typed
    /// would go round between amux and tmux forever.
    pub fn loops_through_tmux(&self, cid: u32) -> Option<String> {
        let c = &self.clients[self.client_idx(cid)?];
        let var = |k: &str| c.env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        let sock = Path::new(var("TMUX")?.split(',').next()?);
        let pane = var("TMUX_PANE")?;
        let (id, _) = self.panes.iter().find(|(_, p)| {
            bridged(&p.meta.argv).is_some_and(|(s, t)| t == pane && util::same_file(Path::new(s), sock))
                && (p.child_pid == 0 || util::proc_cmdline(p.child_pid).is_some_and(|a| a.iter().any(|x| x == "__tmux-bridge")))
        })?;
        Some(format!("this terminal is tmux pane {}, which amux shows live (pane %{}): attaching here would show amux inside itself", pane, id))
    }

    fn import_session(&mut self, socket: &str, ps: &PlanSession) -> Result<String, String> {
        let mut name = ps.name.replace([':', '.'], "_");
        if self.sessions.iter().any(|s| s.name == name) {
            let base = format!("{}-tmux", name);
            name = base.clone();
            let mut k = 2;
            while self.sessions.iter().any(|s| s.name == name) {
                name = format!("{}-{}", base, k);
                k += 1;
            }
        }
        let sid = self.next_session;
        self.next_session += 1;
        let cwd = ps.windows.first().and_then(|w| w.panes.first()).map(|p| p.cwd.clone()).unwrap_or_else(util::home);
        let now = util::now_unix();
        self.sessions.push(Session { id: sid, name: name.clone(), windows: vec![], active: 0, last: None, created: now, activity: now, cwd });
        let (mut live, mut fresh, mut failed) = (0, 0, Vec::new());
        let mut active = None;
        for pw in &ps.windows {
            match self.import_window(sid, socket, &ps.tmux_name, pw) {
                Ok((wid, l, f)) => {
                    live += l;
                    fresh += f;
                    if pw.active || active.is_none() {
                        active = Some(wid);
                    }
                }
                Err(e) => failed.push(format!("window {} ({}): {}", pw.index, pw.name, e)),
            }
        }
        let Some(active) = active else {
            self.sessions.retain(|s| s.id != sid);
            return Err(failed.join("; "));
        };
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) {
            s.active = active;
        }
        let windows = self.sessions.iter().find(|s| s.id == sid).map_or(0, |s| s.windows.len());
        let mut line = format!("{} → amux session {}: {} window(s), {} live pane(s), {} fresh", ps.tmux_name, name, windows, live, fresh);
        for f in failed {
            line.push_str(&format!("\n  not migrated: {}", f));
        }
        logf!("migrate: {}", line);
        Ok(line)
    }

    /// One window: its panes in the tmux layout. Returns (window, live, fresh).
    fn import_window(&mut self, sid: u32, socket: &str, tmux_session: &str, pw: &PlanWindow) -> Result<(u32, usize, usize), String> {
        let cell = tmux::parse_layout(&pw.layout)?;
        let tmux_ids = cell.panes();
        let planned: Vec<u32> = pw.panes.iter().filter_map(|p| p.id.trim_start_matches('%').parse().ok()).collect();
        if tmux_ids.len() != planned.len() || tmux_ids.iter().any(|t| !planned.contains(t)) {
            return Err("its panes changed while migrating; run amux migrate again".into());
        }
        let ids: HashMap<u32, u32> = tmux_ids.iter().map(|t| (*t, self.alloc_pane_id())).collect();
        let layout = cell.to_node(pw.width, &|t| ids.get(&t).copied())?;
        let mut rects = Vec::new();
        layout.layout(Rect { x: 0, y: 0, w: pw.width, h: pw.height }, &mut rects);
        let when = format::strftime("%Y-%m-%d %H:%M");
        let (mut live, mut fresh, mut spawned) = (0, 0, Vec::new());
        for pp in &pw.panes {
            let t: u32 = pp.id.trim_start_matches('%').parse().map_err(|_| format!("bad pane id {}", pp.id))?;
            let id = ids[&t];
            let size = rects.iter().find(|(p, _)| *p == id).map(|(_, r)| (r.w.max(2), r.h.max(1))).unwrap_or((80, 24));
            let (argv, start) = match pp.mode {
                Mode::Live => {
                    live += 1;
                    let mut argv = vec![super::exe_path(), "__tmux-bridge".into(), "-S".into(), socket.into(), "-t".into(), pp.id.clone()];
                    argv.extend(["--pid".into(), pp.pid.to_string(), "--start".into(), pp.start_ticks.to_string()]);
                    (argv, Start { proc_of: Some((pp.pid, pp.start_ticks)), ..Default::default() })
                }
                Mode::Fresh | Mode::Dead => {
                    fresh += 1;
                    let what = if pp.mode == Mode::Dead { "its program had exited" } else { "fresh shell" };
                    let banner = format!("\x1b[0m\x1b[2m── migrated from tmux {} {} ({}), {} ──\x1b[0m\r\n", tmux_session, pp.id, what, when);
                    // An agent that was running is resumed (as after a reboot).
                    let type_in = self.resume_input(self.agent_kinds.detect(&pp.fg_cmd).and_then(|k| k.def.resume.clone()));
                    (Vec::new(), Start { feed_in: pp.feed.clone(), banner: Some(banner), type_in, ..Default::default() })
                }
            };
            let meta = PaneMeta { id, argv, cwd: pp.cwd.clone(), created: util::now_unix(), ..Default::default() };
            match self.spawn_keeper_start(meta, Some(sid), size, start) {
                Ok(_) => spawned.push(id),
                Err(e) => {
                    for p in spawned {
                        self.destroy_pane(p);
                    }
                    return Err(format!("cannot start pane: {}", e));
                }
            }
        }
        let active = pw.panes.iter().find(|p| p.active).and_then(|p| p.id.trim_start_matches('%').parse::<u32>().ok()).and_then(|t| ids.get(&t).copied());
        let wid = self.next_window;
        self.next_window += 1;
        let win = Window {
            id: wid,
            name: pw.name.clone(),
            auto_name: pw.auto_name,
            active: active.unwrap_or(spawned[0]),
            layout,
            last: None,
            zoomed: pw.zoomed,
            sx: pw.width,
            sy: pw.height,
            bell: false,
            activity: false,
        };
        self.windows.insert(wid, win);
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) {
            let idx = if s.window_at(pw.index).is_none() { pw.index } else { s.next_free_index(pw.index) };
            s.insert(idx, wid);
        }
        for p in spawned {
            self.emit_event("pane-new", p, "");
        }
        Ok((wid, live, fresh))
    }
}

/// The tmux socket and pane a bridge pane shows.
fn bridged(argv: &[String]) -> Option<(&str, &str)> {
    if argv.get(1).map(String::as_str) != Some("__tmux-bridge") {
        return None;
    }
    let after = |flag: &str| argv.iter().position(|a| a == flag).and_then(|i| argv.get(i + 1)).map(String::as_str);
    Some((after("-S")?, after("-t")?))
}
