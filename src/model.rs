//! Sessions, windows and the pane layout tree (persisted as JSON).

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Kind {
    Pane(u32),
    Split { horiz: bool, children: Vec<Node> },
}

/// `size` is the node's extent along its parent's split axis.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Node {
    pub size: u16,
    pub kind: Kind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    L,
    R,
    U,
    D,
}

impl Node {
    pub fn pane(id: u32, size: u16) -> Node {
        Node { size, kind: Kind::Pane(id) }
    }

    pub fn panes(&self) -> Vec<u32> {
        let mut v = Vec::new();
        self.collect(&mut v);
        v
    }

    fn collect(&self, v: &mut Vec<u32>) {
        match &self.kind {
            Kind::Pane(p) => v.push(*p),
            Kind::Split { children, .. } => children.iter().for_each(|c| c.collect(v)),
        }
    }

    pub fn contains(&self, id: u32) -> bool {
        match &self.kind {
            Kind::Pane(p) => *p == id,
            Kind::Split { children, .. } => children.iter().any(|c| c.contains(id)),
        }
    }

    /// Compute pane rectangles (borders take one cell between siblings).
    pub fn layout(&self, r: Rect, out: &mut Vec<(u32, Rect)>) {
        match &self.kind {
            Kind::Pane(p) => out.push((*p, r)),
            Kind::Split { horiz, children } => {
                let n = children.len();
                let mut off = 0u16;
                let total = if *horiz { r.w } else { r.h };
                for (i, c) in children.iter().enumerate() {
                    let remaining = total.saturating_sub(off);
                    let size = if i + 1 == n { remaining } else { c.size.min(remaining) };
                    let cr = if *horiz { Rect { x: r.x + off, y: r.y, w: size, h: r.h } } else { Rect { x: r.x, y: r.y + off, w: r.w, h: size } };
                    c.layout(cr, out);
                    off = off.saturating_add(size).saturating_add(1);
                }
            }
        }
    }

    /// Minimum extent along an axis.
    fn min_extent(&self, horiz: bool) -> u16 {
        match &self.kind {
            Kind::Pane(_) => 1,
            Kind::Split { horiz: h, children } => {
                if *h == horiz {
                    children.iter().map(|c| c.min_extent(horiz)).sum::<u16>() + children.len().saturating_sub(1) as u16
                } else {
                    children.iter().map(|c| c.min_extent(horiz)).max().unwrap_or(1)
                }
            }
        }
    }

    /// Rescale children to fill `w`x`h` (proportionally).
    pub fn fit(&mut self, w: u16, h: u16) {
        if let Kind::Split { horiz, children } = &mut self.kind {
            let n = children.len() as u16;
            let total = if *horiz { w } else { h };
            let avail = total.saturating_sub(n.saturating_sub(1)).max(n);
            let sum: u32 = children.iter().map(|c| c.size.max(1) as u32).sum();
            if sum != avail as u32 {
                let mut acc = 0u16;
                let len = children.len();
                for (i, c) in children.iter_mut().enumerate() {
                    if i + 1 == len {
                        c.size = avail.saturating_sub(acc).max(1);
                    } else {
                        let s = ((c.size.max(1) as u32 * avail as u32 + sum / 2) / sum.max(1)).max(1) as u16;
                        let left_for_rest = (len - i - 1) as u16;
                        c.size = s.min(avail.saturating_sub(acc).saturating_sub(left_for_rest)).max(1);
                        acc += c.size;
                    }
                }
            }
            let horiz = *horiz;
            for c in children.iter_mut() {
                if horiz {
                    c.fit(c.size, h);
                } else {
                    c.fit(w, c.size);
                }
            }
        }
    }

    fn find_parent_mut(&mut self, id: u32) -> Option<(&mut Node, usize)> {
        let idx = match &self.kind {
            Kind::Split { children, .. } => children.iter().position(|c| matches!(c.kind, Kind::Pane(p) if p == id)),
            _ => None,
        };
        if let Some(i) = idx {
            return Some((self, i));
        }
        if let Kind::Split { children, .. } = &mut self.kind {
            for c in children.iter_mut() {
                if c.contains(id) {
                    return c.find_parent_mut(id);
                }
            }
        }
        None
    }

    /// Split `target`, adding `new`. `size` is the new pane's extent; `None` halves.
    #[allow(clippy::too_many_arguments)]
    pub fn split(&mut self, target: u32, new: u32, horiz: bool, size: Option<u16>, before: bool, w: u16, h: u16) -> Result<(), String> {
        let mut rects = Vec::new();
        self.layout(Rect { x: 0, y: 0, w, h }, &mut rects);
        let tr = rects.iter().find(|(p, _)| *p == target).map(|(_, r)| *r).ok_or("pane not found")?;
        let total = if horiz { tr.w } else { tr.h };
        if total < 3 {
            return Err("no space for new pane".into());
        }
        let b = size.unwrap_or(total.div_ceil(2) - 1).clamp(1, total - 2);
        let a = total - 1 - b;
        let new_node = Node::pane(new, b);
        // Root is the pane itself.
        if matches!(self.kind, Kind::Pane(p) if p == target) {
            let old = Node::pane(target, a);
            let children = if before { vec![new_node, old] } else { vec![old, new_node] };
            *self = Node { size: self.size, kind: Kind::Split { horiz, children } };
            return Ok(());
        }
        let (parent, i) = self.find_parent_mut(target).ok_or("pane not found")?;
        if let Kind::Split { horiz: ph, children } = &mut parent.kind {
            if *ph == horiz {
                children[i].size = a;
                let at = if before { i } else { i + 1 };
                children.insert(at, new_node);
            } else {
                let keep = children[i].size;
                let old = Node::pane(target, a);
                let sub = if before { vec![new_node, old] } else { vec![old, new_node] };
                children[i] = Node { size: keep, kind: Kind::Split { horiz, children: sub } };
            }
        }
        Ok(())
    }

    /// Remove a pane; its space goes to a neighbour. Returns false if the
    /// root itself is that pane (caller removes the window).
    pub fn remove(&mut self, id: u32) -> bool {
        if matches!(self.kind, Kind::Pane(p) if p == id) {
            return false;
        }
        let Some((parent, i)) = self.find_parent_mut(id) else { return false };
        if let Kind::Split { children, .. } = &mut parent.kind {
            let freed = children[i].size + 1;
            children.remove(i);
            let to = i.saturating_sub(1);
            if let Some(c) = children.get_mut(to) {
                c.size += freed;
            }
            if children.len() == 1 {
                let only = children.pop().unwrap();
                *parent = Node { size: parent.size, kind: only.kind };
            }
        }
        self.flatten();
        true
    }

    /// Merge nested splits with the same orientation.
    fn flatten(&mut self) {
        if let Kind::Split { horiz, children } = &mut self.kind {
            let h = *horiz;
            let mut out = Vec::with_capacity(children.len());
            for mut c in children.drain(..) {
                c.flatten();
                match c.kind {
                    Kind::Split { horiz: ch, children: gc } if ch == h => out.extend(gc),
                    k => out.push(Node { size: c.size, kind: k }),
                }
            }
            *children = out;
        }
    }

    /// Resize a pane by moving one of its borders.
    pub fn resize_pane(&mut self, id: u32, dir: Dir, n: u16, w: u16, h: u16) -> bool {
        let horiz = matches!(dir, Dir::L | Dir::R);
        let ok = self.resize_rec(id, dir, n, horiz);
        self.fit(w, h);
        ok
    }

    fn resize_rec(&mut self, id: u32, dir: Dir, n: u16, axis_h: bool) -> bool {
        let Kind::Split { horiz, children } = &mut self.kind else { return false };
        let Some(i) = children.iter().position(|c| c.contains(id)) else { return false };
        // Prefer the deepest matching split.
        if children[i].resize_rec(id, dir, n, axis_h) {
            return true;
        }
        if *horiz != axis_h || children.len() < 2 {
            return false;
        }
        let grow_toward_next = matches!(dir, Dir::R | Dir::D);
        // Pick the border to move and which side grows.
        let (a, b, grow_a) = if grow_toward_next {
            if i + 1 < children.len() { (i, i + 1, true) } else { (i - 1, i, true) }
        } else if i > 0 {
            (i - 1, i, false)
        } else {
            (i, i + 1, false)
        };
        let (g, s) = if grow_a { (a, b) } else { (b, a) };
        let min_s = children[s].min_extent(axis_h);
        let room = children[s].size.saturating_sub(min_s);
        let d = n.min(room);
        if d == 0 {
            return true;
        }
        children[g].size += d;
        children[s].size -= d;
        true
    }

    pub fn swap(&mut self, a: u32, b: u32) {
        match &mut self.kind {
            Kind::Pane(p) => {
                if *p == a {
                    *p = b;
                } else if *p == b {
                    *p = a;
                }
            }
            Kind::Split { children, .. } => children.iter_mut().for_each(|c| c.swap(a, b)),
        }
    }

    pub fn replace(&mut self, old: u32, new: u32) {
        match &mut self.kind {
            Kind::Pane(p) => {
                if *p == old {
                    *p = new;
                }
            }
            Kind::Split { children, .. } => children.iter_mut().for_each(|c| c.replace(old, new)),
        }
    }

    pub fn even(panes: &[u32], horiz: bool) -> Node {
        if panes.len() == 1 {
            return Node::pane(panes[0], 1);
        }
        Node { size: 1, kind: Kind::Split { horiz, children: panes.iter().map(|&p| Node::pane(p, 1)).collect() } }
    }

    /// Build a preset layout.
    pub fn preset(name: &str, panes: &[u32], w: u16, h: u16, main_w: u16, main_h: u16) -> Option<Node> {
        if panes.is_empty() {
            return None;
        }
        let mut n = match name {
            "even-horizontal" => Node::even(panes, true),
            "even-vertical" => Node::even(panes, false),
            "main-vertical" if panes.len() > 1 => Node {
                size: 1,
                kind: Kind::Split {
                    horiz: true,
                    children: vec![Node::pane(panes[0], main_w.min(w.saturating_sub(2)).max(1)), {
                        let mut r = Node::even(&panes[1..], false);
                        r.size = w.saturating_sub(main_w + 1).max(1);
                        r
                    }],
                },
            },
            "main-horizontal" if panes.len() > 1 => Node {
                size: 1,
                kind: Kind::Split {
                    horiz: false,
                    children: vec![Node::pane(panes[0], main_h.min(h.saturating_sub(2)).max(1)), {
                        let mut r = Node::even(&panes[1..], true);
                        r.size = h.saturating_sub(main_h + 1).max(1);
                        r
                    }],
                },
            },
            "main-vertical" | "main-horizontal" => Node::pane(panes[0], 1),
            "tiled" => {
                let cols = (panes.len() as f64).sqrt().ceil() as usize;
                let rows = panes.len().div_ceil(cols);
                let row_nodes: Vec<Node> = (0..rows)
                    .map(|r| {
                        let slice = &panes[r * cols..((r + 1) * cols).min(panes.len())];
                        Node::even(slice, true)
                    })
                    .collect();
                if row_nodes.len() == 1 {
                    row_nodes.into_iter().next().unwrap()
                } else {
                    Node { size: 1, kind: Kind::Split { horiz: false, children: row_nodes } }
                }
            }
            _ => return None,
        };
        // Even distribution before fitting.
        n.equalize(w, h);
        if name.starts_with("main-") && panes.len() > 1 {
            // keep main size
            if let Kind::Split { children, horiz } = &mut n.kind {
                let total = if *horiz { w } else { h };
                let main = if *horiz { main_w } else { main_h }.min(total.saturating_sub(2)).max(1);
                children[0].size = main;
                children[1].size = total.saturating_sub(main + 1).max(1);
            }
        }
        n.fit(w, h);
        Some(n)
    }

    fn equalize(&mut self, w: u16, h: u16) {
        if let Kind::Split { horiz, children } = &mut self.kind {
            let n = children.len() as u16;
            let total = if *horiz { w } else { h };
            let avail = total.saturating_sub(n - 1);
            let each = (avail / n).max(1);
            let horiz = *horiz;
            for c in children.iter_mut() {
                c.size = each;
                if horiz { c.equalize(each, h) } else { c.equalize(w, each) }
            }
        }
    }

    /// tmux-ish layout description (for list-windows / persistence display).
    pub fn describe(&self) -> String {
        match &self.kind {
            Kind::Pane(p) => format!("%{}", p),
            Kind::Split { horiz, children } => {
                let inner: Vec<String> = children.iter().map(|c| format!("{}:{}", c.size, c.describe())).collect();
                if *horiz { format!("{{{}}}", inner.join(",")) } else { format!("[{}]", inner.join(",")) }
            }
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Window {
    pub id: u32,
    pub name: String,
    pub auto_name: bool,
    pub layout: Node,
    pub active: u32,
    pub last: Option<u32>,
    pub zoomed: bool,
    pub sx: u16,
    pub sy: u16,
    #[serde(skip)]
    pub bell: bool,
    #[serde(skip)]
    pub activity: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Session {
    pub id: u32,
    pub name: String,
    /// (index, window id), sorted by index.
    pub windows: Vec<(u32, u32)>,
    pub active: u32,
    pub last: Option<u32>,
    pub created: u64,
    #[serde(default)]
    pub activity: u64,
    #[serde(default)]
    pub cwd: String,
}

impl Session {
    pub fn index_of(&self, win: u32) -> Option<u32> {
        self.windows.iter().find(|(_, w)| *w == win).map(|(i, _)| *i)
    }
    pub fn window_at(&self, idx: u32) -> Option<u32> {
        self.windows.iter().find(|(i, _)| *i == idx).map(|(_, w)| *w)
    }
    pub fn insert(&mut self, idx: u32, win: u32) {
        self.windows.push((idx, win));
        self.windows.sort_by_key(|(i, _)| *i);
    }
    pub fn next_free_index(&self, base: u32) -> u32 {
        let mut i = base;
        while self.windows.iter().any(|(x, _)| *x == i) {
            i += 1;
        }
        i
    }
}

/// Persisted pane metadata (enough to resurrect it after a reboot).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct PaneMeta {
    pub id: u32,
    pub argv: Vec<String>,
    pub cwd: String,
    pub title: String,
    pub created: u64,
    #[serde(default)]
    pub fg_cmd: String,
    #[serde(default)]
    pub agent: Option<String>,
    /// The agent's resume command, kept for resurrection (its definition
    /// may come from a plugin that is not loaded yet at that point).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_resume: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct State {
    pub version: u32,
    pub next_session: u32,
    pub next_window: u32,
    pub next_pane: u32,
    pub sessions: Vec<Session>,
    pub windows: Vec<Window>,
    pub panes: Vec<PaneMeta>,
    #[serde(default)]
    pub saved_at: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rects(n: &Node, w: u16, h: u16) -> Vec<(u32, Rect)> {
        let mut v = Vec::new();
        n.layout(Rect { x: 0, y: 0, w, h }, &mut v);
        v
    }

    #[test]
    fn split_and_layout() {
        let mut n = Node::pane(1, 80);
        n.split(1, 2, true, None, false, 80, 24).unwrap();
        let r = rects(&n, 80, 24);
        assert_eq!(r[0].1, Rect { x: 0, y: 0, w: 40, h: 24 });
        assert_eq!(r[1].1, Rect { x: 41, y: 0, w: 39, h: 24 });
        n.split(2, 3, false, None, false, 80, 24).unwrap();
        let r = rects(&n, 80, 24);
        assert_eq!(r.len(), 3);
        assert_eq!(r[1].1, Rect { x: 41, y: 0, w: 39, h: 12 });
        assert_eq!(r[2].1, Rect { x: 41, y: 13, w: 39, h: 11 });
        // same-orientation split joins the parent
        n.split(1, 4, true, None, false, 80, 24).unwrap();
        if let Kind::Split { children, .. } = &n.kind {
            assert_eq!(children.len(), 3);
        }
        assert_eq!(n.panes(), vec![1, 4, 2, 3]);
    }

    #[test]
    fn remove_collapses() {
        let mut n = Node::pane(1, 80);
        n.split(1, 2, true, None, false, 80, 24).unwrap();
        n.split(2, 3, false, None, false, 80, 24).unwrap();
        assert!(n.remove(3));
        let r = rects(&n, 80, 24);
        assert_eq!(r.len(), 2);
        assert_eq!(r[1].1.h, 24);
        assert!(n.remove(2));
        assert_eq!(n.kind, Kind::Pane(1));
        let r = rects(&n, 80, 24);
        assert_eq!(r[0].1, Rect { x: 0, y: 0, w: 80, h: 24 });
    }

    #[test]
    fn fit_rescales() {
        let mut n = Node::pane(1, 80);
        n.split(1, 2, true, None, false, 80, 24).unwrap();
        n.fit(120, 40);
        let r = rects(&n, 120, 40);
        assert_eq!(r[0].1.w + r[1].1.w + 1, 120);
        assert!((r[0].1.w as i32 - r[1].1.w as i32).abs() <= 2);
    }

    #[test]
    fn resize_moves_border() {
        let mut n = Node::pane(1, 80);
        n.split(1, 2, true, None, false, 80, 24).unwrap();
        n.resize_pane(1, Dir::R, 5, 80, 24);
        let r = rects(&n, 80, 24);
        assert_eq!(r[0].1.w, 45);
        n.resize_pane(2, Dir::R, 3, 80, 24);
        let r = rects(&n, 80, 24);
        assert_eq!(r[0].1.w, 48);
    }

    #[test]
    fn presets() {
        let n = Node::preset("tiled", &[1, 2, 3, 4], 80, 24, 80, 24).unwrap();
        let r = rects(&n, 80, 24);
        assert_eq!(r.len(), 4);
        assert!(r.iter().all(|(_, rc)| rc.w >= 39 && rc.h >= 11));
        let n = Node::preset("main-vertical", &[1, 2, 3], 100, 30, 60, 24).unwrap();
        let r = rects(&n, 100, 30);
        assert_eq!(r[0].1.w, 60);
    }
}
