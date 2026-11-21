//! Composition (panes, borders, status line, overlays) and minimal-diff
//! terminal output for attached clients.

use super::exec::Fv;
use super::plugins::{self, Tone};
use super::{Client, Overlay, Server, ShownImage};
use crate::agents::State as AState;
use crate::cell::*;
use crate::format;
use crate::model::Rect;

pub struct CanvasImage {
    pub pane: u32,
    pub id: u64,
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub preserve_aspect: bool,
    pub b64: std::sync::Arc<str>,
}

impl CanvasImage {
    fn shown(&self) -> ShownImage {
        ShownImage { pane: self.pane, id: self.id, x: self.x, y: self.y, w: self.w, h: self.h }
    }
}

pub struct Canvas {
    pub w: usize,
    pub h: usize,
    pub cells: Vec<Cell>,
    pub clusters: Clusters,
    pub cursor: Option<(usize, usize)>,
    pub shape: u8,
    pub mouse: bool,
    pub title: Option<String>,
    pub images: Vec<CanvasImage>,
    status_new: Option<StatusCache>,
}

/// A client's last expanded status line, reused while nothing it shows can
/// have changed (pane output alone never invalidates it).
pub struct StatusCache {
    generation: u64,
    w: usize,
    sid: u32,
    wid: u32,
    pid: u32,
    table: String,
    cells: Vec<Cell>,
    clusters: Vec<(usize, String)>,
}

impl Canvas {
    /// Reuse a previous frame's allocation.
    fn with_buffer(mut cells: Vec<Cell>, w: usize, h: usize) -> Canvas {
        cells.clear();
        cells.resize(w * h, Cell::EMPTY);
        Canvas { w, h, cells, clusters: Clusters::default(), cursor: None, shape: 0, mouse: false, title: None, images: Vec::new(), status_new: None }
    }

    #[inline]
    fn set(&mut self, x: usize, y: usize, c: Cell) {
        if x < self.w && y < self.h {
            self.cells[y * self.w + x] = c;
        }
    }

    /// Write a string; returns the next x.
    pub fn text(&mut self, mut x: usize, y: usize, s: &str, st: Style, max_x: usize) -> usize {
        let max_x = max_x.min(self.w);
        for ch in s.chars() {
            if ch.is_control() {
                continue;
            }
            let w = char_width(ch);
            if w == 0 {
                continue;
            }
            if x + w > max_x {
                break;
            }
            self.set(x, y, Cell::with_style(ch as u32, st));
            if w == 2 {
                self.set(x + 1, y, Cell::with_style(WIDE_SPACER, st));
            }
            x += w;
        }
        x
    }

    fn fill(&mut self, x0: usize, y: usize, x1: usize, st: Style) {
        for x in x0..x1.min(self.w) {
            self.set(x, y, Cell::with_style(BLANK, st));
        }
    }
}

pub fn terminal_setup(mouse: bool, focus: bool) -> Vec<u8> {
    let mut s = String::from("\x1b[?1049h\x1b[H\x1b[2J\x1b[?25l\x1b[?7l\x1b[?2004h");
    if mouse {
        s.push_str("\x1b[?1000h\x1b[?1002h\x1b[?1006h");
    }
    if focus {
        s.push_str("\x1b[?1004h");
    }
    s.into_bytes()
}

pub fn terminal_reset() -> Vec<u8> {
    b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1004l\x1b[?2004l\x1b[?7h\x1b[0m\x1b[ q\x1b[?25h\x1b[?1049l".to_vec()
}

fn style_of(spec: &str, base: Style) -> Style {
    format::apply_style(spec, base).0
}

/// Compose and diff for client `ci`.
pub fn render(srv: &mut Server, ci: usize) -> Vec<u8> {
    let spare = std::mem::take(&mut srv.clients[ci].spare);
    let mut cv = {
        let _t = crate::util::prof::time(crate::util::prof::BUILD);
        build(srv, &srv.clients[ci], spare)
    };
    let _t = crate::util::prof::time(crate::util::prof::DIFF);
    let tc_opt = {
        let to = srv.gopt("terminal-overrides");
        let tf = srv.gopt("terminal-features");
        to.contains("Tc") || to.contains("RGB") || tf.contains("RGB")
    };
    let c = &mut srv.clients[ci];
    c.truecolor |= tc_opt;
    if let Some(sc) = cv.status_new.take() {
        c.status_cache = Some(sc);
    }
    emit(c, cv)
}

fn draw_status_cached(srv: &Server, c: &Client, sid: u32, cv: &mut Canvas, row: usize) {
    if c.prompt.is_some() || c.message.is_some() {
        draw_status(srv, c, sid, cv, row);
        return;
    }
    let wid = srv.sessions.iter().find(|s| s.id == sid).map(|s| s.active).unwrap_or(0);
    let pid = srv.windows.get(&wid).map(|w| w.active).unwrap_or(0);
    let w = cv.w;
    let start = row * w;
    if let Some(sc) = &c.status_cache
        && sc.generation == srv.status_gen
        && sc.w == w
        && sc.sid == sid
        && sc.wid == wid
        && sc.pid == pid
        && sc.table == c.key_table
    {
        cv.cells[start..start + w].copy_from_slice(&sc.cells);
        for (x, s) in &sc.clusters {
            cv.cells[start + x].ch = cv.clusters.intern(s);
        }
        return;
    }
    draw_status(srv, c, sid, cv, row);
    let cells = cv.cells[start..start + w].to_vec();
    let clusters = cells
        .iter()
        .enumerate()
        .filter(|(_, cell)| cell.ch & CLUSTER_BIT != 0)
        .map(|(x, cell)| (x, cv.clusters.get(cell.ch).unwrap_or(" ").to_string()))
        .collect();
    cv.status_new = Some(StatusCache { generation: srv.status_gen, w, sid, wid, pid, table: c.key_table.clone(), cells, clusters });
}

fn build(srv: &Server, c: &Client, spare: Vec<Cell>) -> Canvas {
    let (w, h) = (c.cols as usize, c.rows as usize);
    let mut cv = Canvas::with_buffer(spare, w, h);
    let Some(sid) = c.attached else { return cv };
    let Some(sess) = srv.sessions.iter().find(|s| s.id == sid) else { return cv };
    cv.mouse = srv.opt(Some(sid), None, "mouse") == "on";
    let status_on = srv.opt(Some(sid), None, "status") != "off";
    let top = srv.opt(Some(sid), None, "status-position") == "top";
    let (wy0, wh) = if status_on { (if top { 1 } else { 0 }, h.saturating_sub(1)) } else { (0, h) };
    let area = Rect { x: 0, y: wy0 as u16, w: w as u16, h: wh as u16 };
    match &c.overlay {
        Some(Overlay::Agents { sel, tiles, filter }) => draw_agents(srv, &mut cv, area, *sel, *tiles, filter),
        Some(Overlay::Tree { sel, expanded, preview }) => draw_tree(srv, &mut cv, area, Some(sid), *sel, expanded, *preview),
        Some(Overlay::View { title, lines, top }) => draw_view(&mut cv, area, title, lines, *top),
        Some(Overlay::Plugins(v)) => draw_plugins(srv, &mut cv, area, v),
        _ => draw_window(srv, c, &mut cv, sess.active, area),
    }
    if status_on && h > 0 {
        let row = if top { 0 } else { h - 1 };
        draw_status_cached(srv, c, sid, &mut cv, row);
    }
    if srv.gflag("set-titles") {
        let fv = Fv::for_client(srv, c);
        cv.title = Some(format::expand(&srv.gopt("set-titles-string"), &fv));
    }
    cv
}

/// Blit `rects` (relative to `area`) and their split borders onto `cv`.
/// When `scroll_to_tail` is true, taller panes scroll down so active bottom lines remain visible.
fn blit_panes(srv: &Server, cv: &mut Canvas, wid: u32, area: Rect, rects: &[(u32, Rect)], active_pane: u32, scroll_to_tail: bool) {
    let (ox, oy, aw, ah) = (area.x as usize, area.y as usize, area.w as usize, area.h as usize);
    if aw == 0 || ah == 0 {
        return;
    }
    let mut covered = vec![false; aw * ah];
    for (pid, r) in rects {
        let Some(p) = srv.panes.get(pid) else { continue };
        let m = &p.mirror;
        let (rx, rw) = (r.x as usize, r.w as usize);
        if rx >= aw || ox + rx >= cv.w {
            continue;
        }
        let vw = rw.min(aw - rx).min(cv.w - ox - rx);
        let y_start = if !scroll_to_tail || (m.modes.alt_screen && m.cy < r.h as usize) {
            0
        } else {
            let mut last = m.rows;
            while last > 0 && m.row(last - 1).is_empty() {
                last -= 1;
            }
            if m.cursor_visible && last > 0 {
                last = last.max((m.cy + 1).min(m.rows));
            }
            last.saturating_sub(r.h as usize)
        };
        for yy in 0..r.h as usize {
            let y = r.y as usize + yy;
            if y >= ah || oy + y >= cv.h {
                break;
            }
            covered[y * aw + rx..y * aw + rx + vw].fill(true);
            let dst = (oy + y) * cv.w + ox + rx;
            let row = m.row(y_start + yy);
            let n = vw.min(row.len());
            if n > 0 {
                let src = &row[..n];
                cv.cells[dst..dst + n].copy_from_slice(src);
                // Clusters live in the pane's table: re-intern into the canvas.
                if has_cluster(src) {
                    for i in dst..dst + n {
                        let ch = cv.cells[i].ch;
                        if ch & CLUSTER_BIT != 0 {
                            cv.cells[i].ch = cv.clusters.intern(m.clusters.get(ch).unwrap_or(" "));
                        }
                    }
                }
            }
            cv.cells[dst + n..dst + vw].fill(Cell::EMPTY);
            if vw > 0 {
                if cv.cells[dst].ch == WIDE_SPACER {
                    cv.cells[dst].ch = b' ' as u32;
                }
                // A wide char cut by the pane edge becomes a blank.
                let last = cv.cells[dst + vw - 1];
                if last.ch != WIDE_SPACER && last.ch != BLANK && cell_width(&last, &cv.clusters) == 2 {
                    cv.cells[dst + vw - 1].ch = b' ' as u32;
                }
            }
        }
    }
    if rects.len() <= 1 {
        return;
    }
    let sid = srv.session_of_window(wid);
    let normal = style_of(&srv.opt(sid, Some(wid), "pane-border-style"), Style::DEFAULT);
    let active = style_of(&srv.opt(sid, Some(wid), "pane-active-border-style"), Style::DEFAULT);
    let ar = rects.iter().find(|(p, _)| *p == active_pane).map(|(_, r)| *r);
    let is_b = |x: isize, y: isize| -> bool { x >= 0 && y >= 0 && (x as usize) < aw && (y as usize) < ah && !covered[y as usize * aw + x as usize] };
    for y in 0..ah {
        for x in 0..aw {
            if covered[y * aw + x] {
                continue;
            }
            let (xi, yi) = (x as isize, y as isize);
            let (u, d, l, r) = (is_b(xi, yi - 1), is_b(xi, yi + 1), is_b(xi - 1, yi), is_b(xi + 1, yi));
            let ch = match (u || d, l || r) {
                (true, false) => '│',
                (false, true) => '─',
                (false, false) => '·',
                (true, true) => match (u, d, l, r) {
                    (true, true, true, true) => '┼',
                    (true, true, false, true) => '├',
                    (true, true, true, false) => '┤',
                    (false, true, true, true) => '┬',
                    (true, false, true, true) => '┴',
                    (false, true, false, true) => '┌',
                    (false, true, true, false) => '┐',
                    (true, false, false, true) => '└',
                    (true, false, true, false) => '┘',
                    _ => '┼',
                },
            };
            let act = ar.is_some_and(|a| {
                let (ax, ay, aw2, ah2) = (a.x as isize - 1, a.y as isize - 1, a.w as isize + 2, a.h as isize + 2);
                xi >= ax && xi < ax + aw2 && yi >= ay && yi < ay + ah2
            });
            cv.set(ox + x, oy + y, Cell::with_style(ch as u32, if act { active } else { normal }));
        }
    }
}

fn draw_window(srv: &Server, c: &Client, cv: &mut Canvas, wid: u32, area: Rect) {
    let Some(win) = srv.windows.get(&wid) else { return };
    let rects = srv.pane_rects(wid);
    let ox = area.x as usize;
    let oy = area.y as usize;
    let aw = (area.w as usize).min(win.sx as usize);
    let ah = (area.h as usize).min(win.sy as usize);
    let mouse_wanted = srv.panes.get(&win.active).map(|p| p.mirror.modes.mouse != 0).unwrap_or(false);
    cv.mouse |= mouse_wanted;
    blit_panes(srv, cv, wid, Rect { x: area.x, y: area.y, w: aw as u16, h: ah as u16 }, &rects, win.active, false);
    if c.overlay.is_none() {
        for (pid, r) in &rects {
            let Some(p) = srv.panes.get(pid).filter(|p| !p.in_copy) else { continue };
            if r.x as usize >= aw || r.y as usize >= ah {
                continue;
            }
            let vw = r.w.min((aw - r.x as usize) as u16);
            let vh = r.h.min((ah - r.y as usize) as u16);
            for img in &p.mirror.images {
                if img.col >= vw || img.row >= vh {
                    continue;
                }
                let iw = img.w.min(vw - img.col);
                let ih = img.h.min(vh - img.row);
                if iw > 0 && ih > 0 {
                    cv.images.push(CanvasImage {
                        pane: *pid,
                        id: img.id,
                        x: area.x + r.x + img.col,
                        y: area.y + r.y + img.row,
                        w: iw,
                        h: ih,
                        preserve_aspect: img.preserve_aspect,
                        b64: img.b64.clone(),
                    });
                }
            }
        }
    }
    let ar = rects.iter().find(|(p, _)| *p == win.active).map(|(_, r)| *r);
    // display-panes overlay
    if let Some(Overlay::Panes { .. }) = &c.overlay {
        let base = srv.gnum("pane-base-index", 0).max(0) as usize;
        let col = format::parse_colour(&srv.gopt("display-panes-colour")).unwrap_or(color_idx(4));
        let acol = format::parse_colour(&srv.gopt("display-panes-active-colour")).unwrap_or(color_idx(1));
        let order = win.layout.panes();
        for (pid, r) in &rects {
            let idx = order.iter().position(|p| p == pid).unwrap_or(0) + base;
            let label = format!(" {} ", idx);
            let st = Style { fg: color_idx(15) | BOLD, bg: if *pid == win.active { acol } else { col } };
            let x = ox + r.x as usize + (r.w as usize).saturating_sub(label.len()) / 2;
            let y = oy + r.y as usize + r.h as usize / 2;
            cv.text(x, y, &label, st, ox + (r.x + r.w) as usize);
        }
    }
    // Cursor of the active pane.
    if c.overlay.is_none()
        && let (Some(a), Some(p)) = (ar, srv.panes.get(&win.active))
        && p.mirror.cursor_visible
        && p.mirror.cx < a.w as usize
        && p.mirror.cy < a.h as usize
    {
        let (x, y) = (ox + a.x as usize + p.mirror.cx, oy + a.y as usize + p.mirror.cy);
        if x < ox + aw && y < oy + ah {
            cv.cursor = Some((x, y));
            cv.shape = p.mirror.modes.cursor_shape;
        }
    }
}

fn apply_bg_fg(srv: &Server, sid: Option<u32>, wid: Option<u32>, st: &mut Style, bg_key: &str, fg_key: &str) {
    let bg = srv.opt(sid, wid, bg_key);
    if !bg.is_empty()
        && !bg.eq_ignore_ascii_case("default")
        && let Some(c) = format::parse_colour(&bg)
    {
        st.set_bg(c);
    }
    let fg = srv.opt(sid, wid, fg_key);
    if !fg.is_empty()
        && !fg.eq_ignore_ascii_case("default")
        && let Some(c) = format::parse_colour(&fg)
    {
        st.set_fg(c);
    }
}

fn draw_status(srv: &Server, c: &Client, sid: u32, cv: &mut Canvas, row: usize) {
    let w = cv.w;
    let (mut base, _, fill) = format::apply_style(&srv.opt(Some(sid), None, "status-style"), Style::DEFAULT);
    apply_bg_fg(srv, Some(sid), None, &mut base, "status-bg", "status-fg");
    let mut fill_st = base;
    if let Some(f) = fill {
        fill_st.set_bg(f);
    }
    cv.fill(0, row, w, fill_st);
    if let Some(p) = &c.prompt {
        let st = style_of(&srv.opt(Some(sid), None, "message-style"), base);
        cv.fill(0, row, w, st);
        let x = cv.text(0, row, &p.label, st, w);
        cv.text(x, row, &p.text, st, w);
        let before: String = p.text.chars().take(p.cursor).collect();
        let cx = x + str_width(&before);
        cv.cursor = Some((cx.min(w - 1), row));
        cv.shape = 0;
        return;
    }
    if let Some((m, _)) = &c.message {
        let st = style_of(&srv.opt(Some(sid), None, "message-style"), base);
        cv.fill(0, row, w, st);
        cv.text(0, row, m, st, w);
        return;
    }
    let Some(sess) = srv.sessions.iter().find(|s| s.id == sid) else { return };
    let fv = Fv::for_client(srv, c);
    let draw_spans = |cv: &mut Canvas, x: usize, s: &str, st: Style, max: usize| -> usize {
        let (sp, _) = format::spans(s, st);
        let mut x = x;
        for span in sp {
            x = cv.text(x, row, &span.text, span.style, max);
        }
        x
    };
    // Left.
    let left_len = srv.opt(Some(sid), None, "status-left-length").parse::<usize>().unwrap_or(10);
    let left = format::expand(&format::strftime(&srv.opt(Some(sid), None, "status-left")), &fv);
    let lstyle = style_of(&srv.opt(Some(sid), None, "status-left-style"), base);
    let lx = draw_spans(cv, 0, &left, lstyle, left_len.min(w));
    // Right.
    let right_len = srv.opt(Some(sid), None, "status-right-length").parse::<usize>().unwrap_or(40);
    let mut right_fmt = srv.opt(Some(sid), None, "status-right");
    if srv.gflag("agent-badge") && !right_fmt.contains("amux_agents") {
        right_fmt = format!("#{{amux_agents}}{}", right_fmt);
    }
    if !right_fmt.contains("amux_plugins") && !srv.plugins.status_text().is_empty() {
        right_fmt = format!("#{{amux_plugins}}{}", right_fmt);
    }
    let right = format::expand(&format::strftime(&right_fmt), &fv);
    let rstyle = style_of(&srv.opt(Some(sid), None, "status-right-style"), base);
    let (rsp, _) = format::spans(&right, rstyle);
    let rw: usize = rsp.iter().map(|s| str_width(&s.text)).sum::<usize>().min(right_len).min(w.saturating_sub(lx));
    let rx = w - rw;
    {
        let mut x = rx;
        for span in &rsp {
            x = cv.text(x, row, &span.text, span.style, w);
        }
    }
    // Window list.
    let sep = srv.opt(Some(sid), None, "window-status-separator");
    let mut x = lx;
    for (k, (idx, wid)) in sess.windows.iter().enumerate() {
        let Some(win) = srv.windows.get(wid) else { continue };
        let current = *wid == sess.active;
        let fmt = srv.opt(Some(sid), Some(*wid), if current { "window-status-current-format" } else { "window-status-format" });
        let mut st = style_of(&srv.opt(Some(sid), Some(*wid), "window-status-style"), base);
        apply_bg_fg(srv, Some(sid), Some(*wid), &mut st, "window-status-bg", "window-status-fg");
        if current {
            st = style_of(&srv.opt(Some(sid), Some(*wid), "window-status-current-style"), st);
            apply_bg_fg(srv, Some(sid), Some(*wid), &mut st, "window-status-current-bg", "window-status-current-fg");
        } else if win.bell {
            st = style_of(&srv.opt(Some(sid), Some(*wid), "window-status-bell-style"), st);
        } else if win.activity && srv.opt(Some(sid), Some(*wid), "monitor-activity") == "on" {
            st = style_of(&srv.opt(Some(sid), Some(*wid), "window-status-activity-style"), st);
        } else if sess.last == Some(*wid) {
            st = style_of(&srv.opt(Some(sid), Some(*wid), "window-status-last-style"), st);
        }
        let wfv = Fv { srv, client: Some(c), sid: Some(sid), wid: Some(*wid), pid: Some(win.active), idx: Some(*idx) };
        let text = format::expand(&fmt, &wfv);
        if k > 0 {
            x = cv.text(x, row, &sep, base, rx);
        }
        x = draw_spans(cv, x, &text, st, rx);
        if x >= rx {
            break;
        }
    }
}

/// Agent badge for the status line: counts by state.
pub fn agent_badge(srv: &Server) -> String {
    let mut counts = [0usize; 5];
    for p in srv.panes.values() {
        if p.agent.agent.is_some() {
            counts[p.agent.state as usize] += 1;
        }
    }
    let mut s = String::new();
    for st in [AState::Waiting, AState::Working, AState::Done] {
        let n = counts[st as usize];
        if n > 0 {
            s.push_str(&format!("#[fg=colour{},bold]{}{}#[default] ", st.colour(), st.icon(), n));
        }
    }
    s
}

pub struct AgentRow {
    pub pane: u32,
    pub session: u32,
    pub window: u32,
    pub state: AState,
}

/// Agent panes sorted by urgency.
pub fn agent_rows(srv: &Server, filter: &str) -> Vec<AgentRow> {
    let mut rows = Vec::new();
    for s in &srv.sessions {
        for (_, wid) in &s.windows {
            let Some(w) = srv.windows.get(wid) else { continue };
            for pid in w.layout.panes() {
                let Some(p) = srv.panes.get(&pid) else { continue };
                if p.agent.agent.is_none() {
                    continue;
                }
                if !filter.is_empty() {
                    let hay = format!("{} {} {} {}", p.current_command(), p.meta.cwd, p.agent.topic, p.agent.summary).to_lowercase();
                    if !hay.contains(&filter.to_lowercase()) {
                        continue;
                    }
                }
                rows.push(AgentRow { pane: pid, session: s.id, window: *wid, state: p.agent.state });
            }
        }
    }
    rows.sort_by_key(|r| (r.state, std::cmp::Reverse(srv.panes.get(&r.pane).map(|p| p.agent.since).unwrap_or_else(std::time::Instant::now))));
    rows
}

fn ago(t: std::time::Instant) -> String {
    let s = t.elapsed().as_secs();
    if s < 60 {
        format!("{}s", s)
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h", s / 3600)
    }
}

fn short_path(p: &str) -> String {
    let home = crate::util::home();
    let p = if let Some(r) = p.strip_prefix(&home) { format!("~{}", r) } else { p.to_string() };
    let parts: Vec<&str> = p.split('/').collect();
    if parts.len() > 3 { format!("…/{}", parts[parts.len() - 2..].join("/")) } else { p }
}

fn draw_agents(srv: &Server, cv: &mut Canvas, area: Rect, sel: usize, tiles: bool, filter: &str) {
    let (x0, y0, w, h) = (area.x as usize, area.y as usize, area.w as usize, area.h as usize);
    let rows = agent_rows(srv, filter);
    let head = Style { fg: color_idx(15) | BOLD, bg: color_idx(236) };
    cv.fill(x0, y0, x0 + w, head);
    let mut counts = [0usize; 5];
    for r in &rows {
        counts[r.state as usize] += 1;
    }
    let mut title = String::from(" AGENTS ");
    for st in [AState::Waiting, AState::Working, AState::Done, AState::Idle, AState::Exited] {
        if counts[st as usize] > 0 {
            title.push_str(&format!(" {} {} {} ", st.icon(), counts[st as usize], st.label().to_lowercase()));
        }
    }
    if !filter.is_empty() {
        title.push_str(&format!("  filter: {}", filter));
    }
    cv.text(x0, y0, &title, head, x0 + w);
    let hint = " enter:jump  r:reply  1-9:pick  t:tiles  /:filter  x:kill  q:back ";
    let fy = y0 + h - 1;
    let foot = Style { fg: color_idx(250), bg: color_idx(236) };
    cv.fill(x0, fy, x0 + w, foot);
    cv.text(x0, fy, hint, foot, x0 + w);
    if rows.is_empty() {
        let dim = Style { fg: color_idx(245), bg: 0 };
        let msg = [
            "No agents detected.",
            "",
            "Start claude, codex, copilot, gemini, qwen, opencode, pi or aider in any pane,",
            "or mark a pane yourself:  amux status set working \"what it's doing\"",
        ];
        for (i, l) in msg.iter().enumerate() {
            cv.text(x0 + 2, y0 + 2 + i, l, dim, x0 + w);
        }
        return;
    }
    let sel = sel.min(rows.len() - 1);
    if tiles {
        draw_agent_tiles(srv, cv, Rect { x: area.x, y: area.y + 1, w: area.w, h: area.h.saturating_sub(2) }, &rows, sel);
        return;
    }
    let list_h = (h.saturating_sub(4) / 2).max(1).min(rows.len());
    let start = sel.saturating_sub(list_h.saturating_sub(1));
    for (i, r) in rows.iter().enumerate().skip(start).take(list_h) {
        let y = y0 + 1 + (i - start);
        let Some(p) = srv.panes.get(&r.pane) else { continue };
        let selected = i == sel;
        let bg = if selected { color_idx(238) } else { 0 };
        let st_style = Style { fg: color_idx(r.state.colour()) | BOLD, bg };
        let txt = Style { fg: color_idx(252), bg };
        let dim = Style { fg: color_idx(244), bg };
        cv.fill(x0, y, x0 + w, Style { fg: 0, bg });
        let num = if i < 9 { format!("{} ", i + 1) } else { "  ".into() };
        let mut x = cv.text(x0, y, &num, dim, x0 + w);
        x = cv.text(x, y, &format!("{} {:<8}", r.state.icon(), r.state.label()), st_style, x0 + w);
        let sess = srv.sessions.iter().find(|s| s.id == r.session).map(|s| s.name.as_str()).unwrap_or("?");
        let widx = srv.sessions.iter().find(|s| s.id == r.session).and_then(|s| s.index_of(r.window)).unwrap_or(0);
        x = cv.text(x + 1, y, &format!("{:<8}", p.current_command()), Style { fg: color_idx(117) | BOLD, bg }, x0 + w);
        x = cv.text(x + 1, y, &format!("{}:{}.%{}", sess, widx, r.pane), dim, x0 + w);
        x = cv.text(x + 1, y, &short_path(&p.meta.cwd), dim, x0 + w);
        x = cv.text(x + 1, y, &ago(p.agent.since), dim, x0 + w);
        x = cv.text(x + 1, y, &p.agent.sparkline(10), Style { fg: color_idx(r.state.colour()), bg }, x0 + w);
        x += 2;
        // Plugin annotations (short tags) come first: summaries get cut.
        for (_, tag) in srv.plugins.annotations(r.pane) {
            x = cv.text(x, y, &format!("‹{}›", tag), Style { fg: color_idx(180), bg }, x0 + w);
            x = cv.text(x, y, " ", txt, x0 + w);
        }
        if !p.agent.topic.is_empty() && p.agent.topic != p.agent.summary {
            x = cv.text(x, y, &p.agent.topic, Style { fg: color_idx(255) | BOLD, bg }, x0 + w);
            x = cv.text(x, y, " · ", dim, x0 + w);
        }
        cv.text(x, y, &p.agent.summary, txt, x0 + w);
    }
    // Preview of the selected agent's screen.
    let py = y0 + 1 + list_h;
    let ph = (y0 + h - 1).saturating_sub(py);
    if ph < 3 {
        return;
    }
    let border = Style { fg: color_idx(240), bg: 0 };
    let r = &rows[sel];
    let label = format!("─ preview %{} ", r.pane);
    cv.fill(x0, py, x0 + w, border);
    for x in x0..x0 + w {
        cv.set(x, py, Cell::with_style('─' as u32, border));
    }
    cv.text(x0 + 1, py, &label, border, x0 + w);
    let Some(p) = srv.panes.get(&r.pane) else { return };
    draw_mirror_tail(&p.mirror, cv, x0, py + 1, w, ph - 1);
}

/// Draw up to `max_rows` bottom-most non-empty rows of a pane's mirror.
fn draw_mirror_tail(m: &super::Mirror, cv: &mut Canvas, x0: usize, y0: usize, w: usize, max_rows: usize) {
    if w == 0 || max_rows == 0 {
        return;
    }
    let mut last = m.rows;
    while last > 0 && m.row_text(last - 1).is_empty() {
        last -= 1;
    }
    let first = last.saturating_sub(max_rows);
    for (k, yy) in (first..last).enumerate() {
        for xx in 0..m.cols.min(w) {
            let mut cell = m.cell(yy, xx);
            if cell.ch & CLUSTER_BIT != 0 {
                cell.ch = cv.clusters.intern(m.clusters.get(cell.ch).unwrap_or(" "));
            }
            if xx + 1 == w && cell.ch != BLANK && cell.ch != WIDE_SPACER && cell_width(&cell, &cv.clusters) == 2 {
                cell.ch = b' ' as u32;
            }
            cv.set(x0 + xx, y0 + k, cell);
        }
    }
}

fn draw_agent_tiles(srv: &Server, cv: &mut Canvas, area: Rect, rows: &[AgentRow], sel: usize) {
    let n = rows.len();
    let cols = (n as f64).sqrt().ceil() as usize;
    let grid_rows = n.div_ceil(cols);
    let tw = (area.w as usize) / cols.max(1);
    let th = (area.h as usize) / grid_rows.max(1);
    if tw < 10 || th < 3 {
        return;
    }
    for (i, r) in rows.iter().enumerate() {
        let (gx, gy) = (i % cols, i / cols);
        let (x0, y0) = (area.x as usize + gx * tw, area.y as usize + gy * th);
        let Some(p) = srv.panes.get(&r.pane) else { continue };
        let selected = i == sel;
        let hb = if selected { color_idx(r.state.colour()) } else { color_idx(237) };
        let head = Style { fg: if selected { color_idx(16) } else { color_idx(r.state.colour()) } | BOLD, bg: hb };
        cv.fill(x0, y0, x0 + tw - 1, head);
        let title = format!(
            " {}{} {} %{} · {} · {}",
            if i < 9 { format!("{} ", i + 1) } else { String::new() },
            r.state.icon(),
            p.current_command(),
            r.pane,
            short_path(&p.meta.cwd),
            ago(p.agent.since)
        );
        cv.text(x0, y0, &title, head, x0 + tw - 1);
        let sum = Style { fg: color_idx(252), bg: 0 };
        let mut sx = x0 + 1;
        if !p.agent.topic.is_empty() && p.agent.topic != p.agent.summary {
            sx = cv.text(sx, y0 + 1, &p.agent.topic, Style { fg: color_idx(255) | BOLD, bg: 0 }, x0 + tw - 1);
            sx = cv.text(sx, y0 + 1, " · ", Style { fg: color_idx(244), bg: 0 }, x0 + tw - 1);
        }
        cv.text(sx, y0 + 1, &p.agent.summary, sum, x0 + tw - 1);
        draw_mirror_tail(&p.mirror, cv, x0, y0 + 2, tw - 1, th.saturating_sub(3));
    }
}

fn tone_colour(t: Tone) -> u8 {
    match t {
        Tone::Normal => 252,
        Tone::Good => 42,
        Tone::Busy => 214,
        Tone::Bad => 203,
        Tone::Dim => 244,
        Tone::Note => 110,
        Tone::Out => 117,
    }
}

/// A horizontal rule with a label.
fn rule(cv: &mut Canvas, x0: usize, y: usize, w: usize, label: &str) {
    let st = Style { fg: color_idx(240), bg: 0 };
    for x in x0..x0 + w {
        cv.set(x, y, Cell::with_style('─' as u32, st));
    }
    cv.text(x0 + 1, y, label, st, x0 + w);
}

/// The plugin view: plugins and their state, then the selected plugin's
/// summary and the end of its log (or protocol trace, or details).
fn draw_plugins(srv: &Server, cv: &mut Canvas, area: Rect, v: &plugins::View) {
    let (x0, y0, w, h) = (area.x as usize, area.y as usize, area.w as usize, area.h as usize);
    if h < 4 || w < 20 {
        return;
    }
    let rows = srv.plugin_view_rows();
    let head = Style { fg: color_idx(15) | BOLD, bg: color_idx(236) };
    let foot = Style { fg: color_idx(250), bg: color_idx(236) };
    let sel = rows.iter().position(|r| r.name == v.sel).unwrap_or(0);
    let name = rows.get(sel).map(|r| r.name.as_str()).unwrap_or("");
    cv.fill(x0, y0, x0 + w, head);
    let mut title = String::from(" PLUGINS ");
    if v.zoom {
        title.push_str(&format!("› {} › {} ", name, v.show.title()));
    } else {
        for (t, icon, label) in [(Tone::Good, "●", "running"), (Tone::Busy, "◐", "attention"), (Tone::Bad, "✗", "failed"), (Tone::Dim, "○", "off")] {
            let n = rows.iter().filter(|r| r.tone == t).count();
            if n > 0 {
                title.push_str(&format!(" {} {} {} ", icon, n, label));
            }
        }
    }
    let tx = cv.text(x0, y0, &title, head, x0 + w);
    let fy = y0 + h - 1;
    cv.fill(x0, fy, x0 + w, foot);
    let hint = if v.zoom {
        " j/k pgup/pgdn g/G: scroll   t: log/trace/details   enter/q: back "
    } else {
        " j/k: select  enter: expand  t: log/trace/details  r: reload  d: disable/enable  e: edit  x: remove  q: close "
    };
    cv.text(x0, fy, hint, foot, x0 + w);
    if rows.is_empty() {
        let dim = Style { fg: color_idx(245), bg: 0 };
        let msg = [
            format!("No plugins in {}.", srv.plugins.dir.display()),
            String::new(),
            "Create one:  amux plugin new NAME".into(),
            "or ask a coding agent: read `amux plugin guide`, then build an amux plugin that ...".into(),
        ];
        for (i, l) in msg.iter().enumerate() {
            cv.text(x0 + 2, y0 + 2 + i, l, dim, x0 + w);
        }
        return;
    }
    let (body, body_h) = (y0 + 1, h - 2);
    if v.zoom {
        let width = w.saturating_sub(2).max(10);
        let lines: Vec<(String, Tone)> = srv
            .plugin_view_lines(name, v.show, plugins::VIEW_LINES)
            .into_iter()
            .flat_map(|(l, t)| plugins::wrap(&l, width).into_iter().map(move |r| (r, t)))
            .collect();
        let end = lines.len().saturating_sub(body_h);
        let top = v.top.unwrap_or(end).min(end);
        for (k, (l, t)) in lines.iter().skip(top).take(body_h).enumerate() {
            cv.text(x0 + 1, body + k, l, Style { fg: color_idx(tone_colour(*t)), bg: 0 }, x0 + w);
        }
        let pos = if v.top.is_none() { "following ".to_string() } else { format!("{}-{}/{} ", top + 1, (top + body_h).min(lines.len()), lines.len()) };
        if tx + pos.len() < x0 + w {
            cv.text(x0 + w - pos.len(), y0, &pos, head, x0 + w);
        }
        return;
    }
    let dir = short_path(&srv.plugins.dir.display().to_string());
    if tx + dir.chars().count() + 2 < x0 + w {
        cv.text(x0 + w - dir.chars().count() - 1, y0, &dir, Style { fg: color_idx(245), bg: color_idx(236) }, x0 + w);
    }
    // The list takes up to a third of the screen.
    let list_h = rows.len().min((body_h / 3).max(1));
    let start = sel.saturating_sub(list_h - 1);
    for (k, r) in rows.iter().enumerate().skip(start).take(list_h) {
        let y = body + k - start;
        let bg = if k == sel { color_idx(238) } else { 0 };
        cv.fill(x0, y, x0 + w, Style { fg: 0, bg });
        let icon = match r.tone {
            Tone::Good => "●",
            Tone::Busy => "◐",
            Tone::Bad => "✗",
            _ => "○",
        };
        let colour = color_idx(tone_colour(r.tone));
        let mut x = cv.text(x0 + 1, y, icon, Style { fg: colour | BOLD, bg }, x0 + w);
        x = cv.text(x + 1, y, &format!("{:<16}", r.name), Style { fg: color_idx(117) | BOLD, bg }, x0 + w);
        x = cv.text(x + 1, y, &format!("{:<10}", r.state), Style { fg: colour, bg }, x0 + w);
        let info = Style { fg: color_idx(if r.tone == Tone::Bad { 203 } else { 250 }), bg };
        cv.text(x + 1, y, &r.info, info, x0 + w);
    }
    let mut y = body + list_h;
    rule(cv, x0, y, w, &format!(" {} ", name));
    y += 1;
    for (l, t) in srv.plugin_view_summary(name) {
        if y + 3 > fy {
            break;
        }
        cv.text(x0 + 1, y, &l, Style { fg: color_idx(tone_colour(t)), bg: 0 }, x0 + w);
        y += 1;
    }
    if y + 2 > fy {
        return;
    }
    rule(cv, x0, y, w, &format!(" {} ", v.show.title()));
    y += 1;
    for (l, t) in srv.plugin_view_lines(name, v.show, fy - y) {
        cv.text(x0 + 1, y, &l, Style { fg: color_idx(tone_colour(t)), bg: 0 }, x0 + w);
        y += 1;
    }
}

/// Visible tree rows for `choose-tree`, built live from current sessions and windows.
pub fn tree_items(srv: &Server, cur_sid: Option<u32>, expanded: &std::collections::HashSet<u32>) -> Vec<super::TreeItem> {
    let mut items = Vec::new();
    for s in &srv.sessions {
        let open = expanded.contains(&s.id);
        let arrow = if open { "▾" } else { "▸" };
        let wword = if s.windows.len() == 1 { "window" } else { "windows" };
        let att = if Some(s.id) == cur_sid || srv.clients.iter().any(|c| c.attached == Some(s.id)) { " (attached)" } else { "" };
        let mut acounts = [0usize; 5];
        for (_, wid) in &s.windows {
            if let Some(w) = srv.windows.get(wid) {
                for pid in w.layout.panes() {
                    if let Some(p) = srv.panes.get(&pid).filter(|p| p.agent.agent.is_some()) {
                        acounts[p.agent.state as usize] += 1;
                    }
                }
            }
        }
        let mut agents = String::new();
        for st in [AState::Waiting, AState::Working, AState::Done] {
            let n = acounts[st as usize];
            if n > 0 {
                agents.push_str(&format!("  {}{}", st.icon(), n));
            }
        }
        items.push(super::TreeItem { label: format!("{} {}: {} {}{}{}", arrow, s.name, s.windows.len(), wword, att, agents), session: s.id, window: None });
        if open {
            let wn = s.windows.len();
            for (k, (idx, wid)) in s.windows.iter().enumerate() {
                let branch = if k + 1 == wn { "└─" } else { "├─" };
                let flag = if *wid == s.active {
                    "*"
                } else if s.last == Some(*wid) {
                    "-"
                } else {
                    ""
                };
                let panes_n = srv.windows.get(wid).map_or(1, |w| w.layout.panes().len());
                let panes = if panes_n > 1 { format!(" ({} panes)", panes_n) } else { String::new() };
                let agent = srv
                    .windows
                    .get(wid)
                    .and_then(|ww| srv.panes.get(&ww.active))
                    .filter(|p| p.agent.agent.is_some())
                    .map(|p| format!("  {} {}", p.agent.state.icon(), p.agent.state.label().to_lowercase()))
                    .unwrap_or_default();
                items.push(super::TreeItem {
                    label: format!("  {} {}: {}{}{}{}", branch, idx, srv.window_name(*wid), flag, panes, agent),
                    session: s.id,
                    window: Some(*wid),
                });
            }
        }
    }
    items
}

/// Number of rows allocated to the tree list inside `body_h`.
pub fn tree_list_height(body_h: usize, items_len: usize, preview: bool) -> usize {
    if !preview || body_h < 6 {
        return body_h;
    }
    items_len.clamp(1, (body_h * 2 / 5).max(3))
}

fn draw_window_preview(srv: &Server, cv: &mut Canvas, wid: u32, area: Rect) {
    if area.w == 0 || area.h == 0 {
        return;
    }
    let Some(win) = srv.windows.get(&wid) else { return };
    let mut rects = Vec::new();
    if win.zoomed || win.layout.panes().len() <= 1 {
        rects.push((win.active, Rect { x: 0, y: 0, w: area.w, h: area.h }));
    } else {
        let mut scaled = win.layout.clone();
        scaled.fit(area.w.max(2), area.h.max(1));
        scaled.layout(Rect { x: 0, y: 0, w: area.w, h: area.h }, &mut rects);
    }
    blit_panes(srv, cv, wid, area, &rects, win.active, true);
}

fn draw_tree(srv: &Server, cv: &mut Canvas, area: Rect, cur_sid: Option<u32>, sel: usize, expanded: &std::collections::HashSet<u32>, preview: bool) {
    let (x0, y0, w, h) = (area.x as usize, area.y as usize, area.w as usize, area.h as usize);
    if w == 0 || h == 0 {
        return;
    }
    let items = tree_items(srv, cur_sid, expanded);
    let win_total: usize = srv.sessions.iter().map(|s| s.windows.len()).sum();
    let head = Style { fg: color_idx(15) | BOLD, bg: color_idx(236) };
    cv.fill(x0, y0, x0 + w, head);
    cv.text(x0, y0, &format!(" choose-tree · {} sessions · {} windows ", srv.sessions.len(), win_total), head, x0 + w);
    let has_foot = h >= 4;
    if has_foot {
        let fy = y0 + h - 1;
        let foot = Style { fg: color_idx(250), bg: color_idx(236) };
        cv.fill(x0, fy, x0 + w, foot);
        cv.text(x0, fy, " enter/1-9:jump  j/k:move  l/h/space:windows  v:preview  x:kill  q:cancel ", foot, x0 + w);
    }
    let body_h = if has_foot { h - 2 } else { h.saturating_sub(1) };
    if items.is_empty() || body_h == 0 {
        return;
    }
    let sel = sel.min(items.len() - 1);
    let list_h = tree_list_height(body_h, items.len(), preview);
    let start = sel.saturating_sub(list_h.saturating_sub(1));
    for (i, it) in items.iter().enumerate().skip(start).take(list_h) {
        let y = y0 + 1 + i - start;
        let selected = i == sel;
        let bg = if selected { color_idx(3) } else { 0 };
        let num_st = if selected { Style { fg: color_idx(0), bg } } else { Style { fg: color_idx(244), bg } };
        let txt_st = if selected {
            Style { fg: color_idx(0) | BOLD, bg }
        } else if it.window.is_none() {
            Style { fg: color_idx(255) | BOLD, bg }
        } else {
            Style { fg: color_idx(250), bg }
        };
        cv.fill(x0, y, x0 + w, Style { fg: 0, bg });
        let num = if i < 9 { format!("{} ", i + 1) } else { "  ".into() };
        let x = cv.text(x0 + 1, y, &num, num_st, x0 + w);
        cv.text(x, y, &it.label, txt_st, x0 + w);
    }
    if !preview || body_h < 6 {
        return;
    }
    let py = y0 + 1 + list_h;
    let fy_end = if has_foot { y0 + h - 1 } else { y0 + h };
    if py + 2 > fy_end {
        return;
    }
    let it = &items[sel];
    let sess = srv.sessions.iter().find(|s| s.id == it.session);
    let Some(wid) = it.window.or_else(|| sess.map(|s| s.active)) else { return };
    let border = Style { fg: color_idx(240), bg: 0 };
    for x in x0..x0 + w {
        cv.set(x, py, Cell::with_style('─' as u32, border));
    }
    let sname = sess.map(|s| s.name.as_str()).unwrap_or("?");
    let cwd = srv.windows.get(&wid).and_then(|win| srv.panes.get(&win.active)).map(|p| short_path(&p.meta.cwd)).unwrap_or_default();
    let mut x = cv.text(x0 + 1, py, &format!(" preview [{}] ", sname), Style { fg: color_idx(252) | BOLD, bg: 0 }, x0 + w);
    match (it.window, sess) {
        (None, Some(s)) => {
            for (idx, w_id) in &s.windows {
                let active = *w_id == s.active;
                let flag = if active {
                    "*"
                } else if s.last == Some(*w_id) {
                    "-"
                } else {
                    ""
                };
                let st = if active { Style { fg: color_idx(117) | BOLD, bg: 0 } } else { Style { fg: color_idx(246), bg: 0 } };
                x = cv.text(x, py, &format!("{}:{}{}  ", idx, srv.window_name(*w_id), flag), st, x0 + w);
            }
        }
        (Some(w_id), Some(s)) => {
            let idx = s.index_of(w_id).unwrap_or(0);
            let panes_n = srv.windows.get(&w_id).map_or(1, |win| win.layout.panes().len());
            x = cv.text(
                x,
                py,
                &format!("{}:{} ({} pane{}) ", idx, srv.window_name(w_id), panes_n, if panes_n == 1 { "" } else { "s" }),
                Style { fg: color_idx(117) | BOLD, bg: 0 },
                x0 + w,
            );
        }
        _ => {}
    }
    if !cwd.is_empty() && x + cwd.len() + 2 < x0 + w {
        cv.text(x, py, &format!("{} ", cwd), Style { fg: color_idx(244), bg: 0 }, x0 + w);
    }
    draw_window_preview(srv, cv, wid, Rect { x: area.x, y: (py + 1) as u16, w: area.w, h: (fy_end - py - 1) as u16 });
}

fn draw_view(cv: &mut Canvas, area: Rect, title: &str, lines: &[String], top: usize) {
    let (x0, y0, w, h) = (area.x as usize, area.y as usize, area.w as usize, area.h as usize);
    let head = Style { fg: color_idx(0), bg: color_idx(3) };
    cv.fill(x0, y0, x0 + w, head);
    cv.text(x0, y0, &format!(" {}  [{}/{}]  (q to close)", title, top, lines.len()), head, x0 + w);
    for (k, l) in lines.iter().skip(top).take(h.saturating_sub(1)).enumerate() {
        cv.text(x0, y0 + 1 + k, l, Style::DEFAULT, x0 + w);
    }
}

// ----- output ---------------------------------------------------------

fn sgr(st: Style, truecolor: bool, out: &mut Vec<u8>) {
    let mut s = String::from("\x1b[0");
    if st.has(BOLD) {
        s.push_str(";1");
    }
    if st.has(DIM) {
        s.push_str(";2");
    }
    if st.has(ITALIC) {
        s.push_str(";3");
    }
    if st.underline() != 0 {
        s.push_str(";4");
    }
    if st.has(BLINK) {
        s.push_str(";5");
    }
    if st.has(REVERSE) {
        s.push_str(";7");
    }
    if st.has(HIDDEN) {
        s.push_str(";8");
    }
    if st.bg & STRIKE != 0 {
        s.push_str(";9");
    }
    if st.bg & OVERLINE != 0 {
        s.push_str(";53");
    }
    let col = |c: Color, fg: bool, s: &mut String| match c {
        Color::Default => {}
        Color::Idx(i) if i < 8 => s.push_str(&format!(";{}", if fg { 30 } else { 40 } + i as u32)),
        Color::Idx(i) if i < 16 => s.push_str(&format!(";{}", if fg { 90 } else { 100 } + (i - 8) as u32)),
        Color::Idx(i) => s.push_str(&format!(";{};5;{}", if fg { 38 } else { 48 }, i)),
        Color::Rgb(r, g, b) => {
            if truecolor {
                s.push_str(&format!(";{};2;{};{};{}", if fg { 38 } else { 48 }, r, g, b));
            } else {
                s.push_str(&format!(";{};5;{}", if fg { 38 } else { 48 }, rgb_to_256(r, g, b)));
            }
        }
    };
    col(st.fg_color(), true, &mut s);
    col(st.bg_color(), false, &mut s);
    s.push('m');
    out.extend_from_slice(s.as_bytes());
}

fn same(a: Cell, ta: &Clusters, b: Cell, tb: &Clusters) -> bool {
    if a.fg != b.fg || a.bg != b.bg {
        return false;
    }
    if a.ch & CLUSTER_BIT != 0 && b.ch & CLUSTER_BIT != 0 {
        return ta.get(a.ch) == tb.get(b.ch);
    }
    a.ch == b.ch
}

fn emit(c: &mut Client, cv: Canvas) -> Vec<u8> {
    let (w, h) = (cv.w, cv.h);
    let mut out = Vec::with_capacity(4096);
    out.extend_from_slice(b"\x1b[?2026h\x1b[?25l");
    if cv.mouse != c.outer_mouse {
        out.extend_from_slice(if cv.mouse { b"\x1b[?1000h\x1b[?1002h\x1b[?1006h" } else { b"\x1b[?1000l\x1b[?1002l\x1b[?1006l" });
        c.outer_mouse = cv.mouse;
    }
    if let Some(t) = &cv.title
        && *t != c.title
    {
        out.extend_from_slice(format!("\x1b]2;{}\x07", t.replace(['\x07', '\x1b'], "")).as_bytes());
        c.title = t.clone();
    }
    let full = c.full_redraw || c.front_size != (w as u16, h as u16) || c.front.len() != w * h;
    let old_imgs = std::mem::take(&mut c.shown_images);
    if full {
        out.extend_from_slice(b"\x1b[0m\x1b[H\x1b[2J");
    } else {
        for old in &old_imgs {
            if !cv.images.iter().any(|n| n.shown() == *old) {
                for yy in (old.y as usize)..((old.y + old.h) as usize).min(h) {
                    for xx in (old.x as usize)..((old.x + old.w) as usize).min(w) {
                        c.front[yy * w + xx].ch = u32::MAX;
                    }
                }
            }
        }
    }
    let mut cur: Option<(usize, usize)> = None;
    let mut style: Option<Style> = if full { Some(Style::DEFAULT) } else { None };
    let mut text = String::new();
    for y in 0..h {
        if !full {
            let row = y * w..(y + 1) * w;
            // Identical rows without clusters are unchanged (cluster ids are
            // per-canvas, so those rows take the cell-by-cell path).
            if cell_bytes(&cv.cells[row.clone()]) == cell_bytes(&c.front[row.clone()]) && !has_cluster(&cv.cells[row]) {
                continue;
            }
        }
        let mut x = 0;
        while x < w {
            let i = y * w + x;
            let n = cv.cells[i];
            let wide = n.ch != WIDE_SPACER && x + 1 < w && cv.cells[i + 1].ch == WIDE_SPACER;
            let step = if wide { 2 } else { 1 };
            if !full {
                let unchanged = same(n, &cv.clusters, c.front[i], &c.clusters) && (!wide || same(cv.cells[i + 1], &cv.clusters, c.front[i + 1], &c.clusters));
                if unchanged {
                    x += step;
                    continue;
                }
            } else if n == Cell::EMPTY {
                x += 1;
                continue;
            }
            if cur != Some((x, y)) {
                out.extend_from_slice(format!("\x1b[{};{}H", y + 1, x + 1).as_bytes());
            }
            if style != Some(n.style()) {
                sgr(n.style(), c.truecolor, &mut out);
                style = Some(n.style());
            }
            text.clear();
            match n.ch {
                BLANK => text.push(' '),
                WIDE_SPACER => text.push(' '),
                ch if ch & CLUSTER_BIT != 0 => text.push_str(cv.clusters.get(ch).unwrap_or(" ")),
                ch => text.push(char::from_u32(ch).filter(|c| !c.is_control()).unwrap_or(' ')),
            }
            out.extend_from_slice(text.as_bytes());
            // After non-ASCII, the terminal's idea of width may differ: re-sync.
            cur = if text.len() == 1 { Some((x + 1, y)) } else { None };
            x += step;
        }
    }
    for img in &cv.images {
        let s = img.shown();
        if full || !old_imgs.contains(&s) {
            out.extend_from_slice(
                format!(
                    "\x1b[{};{}H\x1b]1337;File=inline=1;width={};height={};preserveAspectRatio={};doNotMoveCursor=1:",
                    img.y + 1,
                    img.x + 1,
                    img.w,
                    img.h,
                    img.preserve_aspect as u8
                )
                .as_bytes(),
            );
            out.extend_from_slice(img.b64.as_bytes());
            out.push(0x07);
        }
    }
    match cv.cursor {
        Some((x, y)) => {
            out.extend_from_slice(format!("\x1b[{};{}H", y + 1, x + 1).as_bytes());
            if c.cursor.3 != cv.shape || full {
                out.extend_from_slice(format!("\x1b[{} q", cv.shape).as_bytes());
            }
            out.extend_from_slice(b"\x1b[?25h");
            c.cursor = (x as u16, y as u16, true, cv.shape);
        }
        None => c.cursor.2 = false,
    }
    out.extend_from_slice(b"\x1b[?2026l");
    c.shown_images = cv.images.iter().map(|i| i.shown()).collect();
    c.spare = std::mem::replace(&mut c.front, cv.cells);
    c.clusters = cv.clusters;
    c.front_size = (w as u16, h as u16);
    c.full_redraw = false;
    out
}
