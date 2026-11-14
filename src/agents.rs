//! Agent awareness: detect coding agents running in panes and infer what they
//! are doing from generic signals (explicit status, OSC progress/notify,
//! bells, screen patterns, output activity).
//!
//! What amux knows about a particular agent CLI (how to recognise it, read its
//! screen and resume it) is data: a built-in table that plugins extend or
//! override while amux runs (`Kinds`). The tracking itself is generic.

use regex_lite::Regex;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A built-in agent definition (see `Def`).
struct AgentDef {
    pub name: &'static str,
    /// Regex matched against the foreground command line.
    pub cmd: &'static str,
    /// Patterns (case-insensitive) that mean "working".
    pub working: &'static [&'static str],
    /// Patterns that mean "waiting for the user" (approval/question).
    pub waiting: &'static [&'static str],
    /// UI chrome lines (status bars, input placeholders) that never summarize work.
    pub chrome: &'static [&'static str],
    /// Command to resume the last conversation after a reboot.
    pub resume: Option<&'static str>,
}

const COMMON_WORKING: &[&str] = &[r"esc to (interrupt|cancel)", r"ctrl\+c to (interrupt|cancel|stop)", r"\(esc to cancel"];
const COMMON_WAITING: &[&str] = &[
    r"do you want to (proceed|continue|make this|create|run|allow|apply|overwrite|edit)",
    r"would you like to",
    r"\b(allow|approve|permit)\b.*\?",
    r"\(y/n\)|\[y/n\]|\(yes/no\)",
    r"waiting for (your )?(approval|confirmation|input)",
    r"press enter to (continue|confirm)",
];

/// Key hints and status bars shared by agent TUIs.
const COMMON_CHROME: &[&str] = &[
    r"\bctrl\+[a-z]",
    r"⌃|⏎",
    r"shift\+tab",
    r"\? for (shortcuts|help)",
    r"esc to (interrupt|cancel|stop)",
    r"context left|tokens? used|auto-accept|bypass permissions|accept edits",
];

static BUILTIN: &[AgentDef] = &[
    AgentDef {
        name: "claude",
        cmd: r"(^|[/ ])claude( |$)|claude-code",
        working: &[],
        waiting: &[r"❯ 1\. yes"],
        chrome: &[],
        resume: Some("claude --continue"),
    },
    AgentDef {
        name: "codex",
        cmd: r"(^|[/ ])codex( |$)|@openai/codex",
        working: &[r"^\s*. working"],
        waiting: &[r"allow command", r"▌ ?yes"],
        chrome: &[r"ask codex to do anything"],
        resume: Some("codex resume --last"),
    },
    AgentDef {
        name: "copilot",
        cmd: r"(^|[/ ])copilot( |$)|@github/copilot",
        working: &[r"thinking", r"esc to stop"],
        waiting: &[r"❯ 1\. yes"],
        chrome: &[r"\[⎇ ", r"to mention files"],
        resume: Some("copilot --continue"),
    },
    AgentDef {
        name: "gemini",
        cmd: r"(^|[/ ])gemini( |$)|gemini-cli",
        working: &[],
        waiting: &[r"apply this change", r"allow execution"],
        chrome: &[],
        resume: None,
    },
    AgentDef {
        name: "qwen",
        cmd: r"(^|[/ ])qwen( |$)|qwen-code",
        working: &[],
        waiting: &[r"apply this change", r"allow execution"],
        chrome: &[],
        resume: None,
    },
    AgentDef {
        name: "opencode",
        cmd: r"(^|[/ ])opencode( |$)",
        working: &[r"working"],
        waiting: &[r"permission required"],
        chrome: &[],
        resume: Some("opencode --continue"),
    },
    AgentDef { name: "pi", cmd: r"(^|[/ ])pi( |$)", working: &[r"working\.\.\."], waiting: &[], chrome: &[], resume: Some("pi --continue") },
    AgentDef { name: "aider", cmd: r"(^|[/ ])aider( |$)", working: &[], waiting: &[r"\(y\)es/\(n\)o", r"add .* to the chat\?"], chrome: &[], resume: None },
    AgentDef { name: "goose", cmd: r"(^|[/ ])goose( |$)", working: &[], waiting: &[], chrome: &[], resume: None },
    AgentDef { name: "amp", cmd: r"(^|[/ ])amp( |$)", working: &[], waiting: &[], chrome: &[], resume: None },
    AgentDef { name: "cursor-agent", cmd: r"cursor-agent", working: &[], waiting: &[], chrome: &[], resume: None },
    AgentDef { name: "crush", cmd: r"(^|[/ ])crush( |$)", working: &[], waiting: &[], chrome: &[], resume: None },
];

/// The kind of panes marked as agents explicitly (`amux status set ...`).
pub const GENERIC: &str = "agent";

impl AgentDef {
    fn def(&self) -> Def {
        let own = |v: &[&str]| v.iter().map(|s| s.to_string()).collect();
        Def {
            name: self.name.into(),
            cmd: self.cmd.into(),
            working: own(self.working),
            waiting: own(self.waiting),
            chrome: own(self.chrome),
            resume: self.resume.map(String::from),
        }
    }
}

/// How to recognise an agent CLI, read its screen and resume it. Built in, or
/// registered by a plugin (`agents` in its `register` request).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Def {
    pub name: String,
    /// Regex matched against the foreground command line: the program and its
    /// first argument (node/bun scripts name the agent there).
    pub cmd: String,
    /// Case-insensitive regexes for screen lines meaning "working".
    #[serde(default)]
    pub working: Vec<String>,
    /// Lines meaning "waiting for the user" (an approval or a question).
    #[serde(default)]
    pub waiting: Vec<String>,
    /// UI chrome (status bars, input placeholders) never used as a summary.
    #[serde(default)]
    pub chrome: Vec<String>,
    /// Typed into a shell to continue the last conversation after a reboot.
    #[serde(default)]
    pub resume: Option<String>,
}

/// A compiled agent definition. Patterns shared by agent TUIs are included.
#[derive(Debug)]
pub struct Kind {
    pub def: Def,
    /// The plugin that registered it; `None` for built-in kinds.
    pub plugin: Option<u32>,
    cmd: Regex,
    working: Vec<Regex>,
    waiting: Vec<Regex>,
    chrome: Vec<Regex>,
}

const MAX_PATTERNS: usize = 32;
const MAX_PATTERN_LEN: usize = 256;

impl Kind {
    /// Validate and compile a definition.
    pub fn compile(def: Def, plugin: Option<u32>) -> Result<Kind, String> {
        let name_ok = !def.name.is_empty() && def.name.len() <= 32 && def.name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
        if !name_ok {
            return Err("invalid name: up to 32 letters, digits, '-', '_' or '.'".into());
        }
        if plugin.is_some() && def.name == GENERIC {
            return Err(format!("reserved name: `{}` marks panes that set their status explicitly", GENERIC));
        }
        if def.cmd.is_empty() || def.cmd.len() > MAX_PATTERN_LEN {
            return Err(format!("cmd: a regex of 1 to {} characters", MAX_PATTERN_LEN));
        }
        let cmd = Regex::new(&def.cmd).map_err(|e| format!("cmd: {}", e))?;
        if cmd.is_match("") || crate::util::SHELLS.iter().any(|sh| cmd.is_match(sh)) {
            return Err(format!("cmd: {:?} would match every command or a plain shell", def.cmd));
        }
        let list = |what: &str, own: &[String], common: &[&str]| -> Result<Vec<Regex>, String> {
            if own.len() > MAX_PATTERNS {
                return Err(format!("{}: at most {} patterns", what, MAX_PATTERNS));
            }
            let mut out = Vec::with_capacity(own.len() + common.len());
            for p in own.iter().map(String::as_str).chain(common.iter().copied()) {
                if p.is_empty() || p.len() > MAX_PATTERN_LEN {
                    return Err(format!("{}: patterns must be 1 to {} characters", what, MAX_PATTERN_LEN));
                }
                let r = Regex::new(&format!("(?i){}", p)).map_err(|e| format!("{}: {:?}: {}", what, p, e))?;
                if r.is_match("") {
                    return Err(format!("{}: {:?} would match every line", what, p));
                }
                out.push(r);
            }
            Ok(out)
        };
        let working = list("working", &def.working, COMMON_WORKING)?;
        let waiting = list("waiting", &def.waiting, COMMON_WAITING)?;
        let chrome = list("chrome", &def.chrome, COMMON_CHROME)?;
        if def.resume.as_ref().is_some_and(|r| r.trim().is_empty() || r.len() > 200 || r.chars().any(char::is_control)) {
            return Err("resume: one line of at most 200 characters (it is typed into a shell)".into());
        }
        Ok(Kind { def, plugin, cmd, working, waiting, chrome })
    }

    pub fn name(&self) -> &str {
        &self.def.name
    }

    /// The kind of panes marked with `amux status set` (no definition matched).
    pub fn is_generic(&self) -> bool {
        self.plugin.is_none() && self.def.name == GENERIC
    }
}

/// Whether two trackers' kinds are the same definition.
pub fn same(a: &Option<Arc<Kind>>, b: &Option<Arc<Kind>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => Arc::ptr_eq(x, y),
        _ => false,
    }
}

/// The words of a command line that name what it runs: `sh -c CMD` wrappers
/// (shells that do not exec the last command) are looked through, then the
/// program and its first argument are kept.
fn head(cmdline: &str) -> Vec<&str> {
    let mut toks: Vec<&str> = cmdline.split_whitespace().collect();
    while toks.len() > 2 && toks[1] == "-c" && crate::util::is_shell(toks[0].rsplit('/').next().unwrap_or("")) {
        toks.drain(..2);
    }
    toks.truncate(2);
    toks
}

/// The program a command line runs (its file name), looking through `sh -c`.
pub fn program(cmdline: &str) -> &str {
    head(cmdline).first().map_or("", |p| p.rsplit('/').next().unwrap_or(p))
}

/// Whether `cmdline` runs the program called `name`, directly or as the
/// script its interpreter runs (`node .../kiro`, `/bin/sh .../agent`).
pub fn runs(cmdline: &str, name: &str) -> bool {
    !name.is_empty() && head(cmdline).iter().any(|w| w.rsplit('/').next() == Some(name))
}

/// The agent kinds amux recognises, in detection order: kinds registered by
/// plugins first (they replace built-in kinds of the same name), then the
/// built-in ones.
pub struct Kinds {
    builtin: Vec<Arc<Kind>>,
    plugin: Vec<Arc<Kind>>,
    active: Vec<Arc<Kind>>,
    generic: Arc<Kind>,
}

impl Default for Kinds {
    fn default() -> Self {
        Self::new()
    }
}

impl Kinds {
    pub fn new() -> Kinds {
        let compile = |d: Def| Arc::new(Kind::compile(d, None).expect("valid built-in agent definition"));
        let generic = Def { name: GENERIC.into(), cmd: r"^\x00never$".into(), ..Default::default() };
        let mut k = Kinds { builtin: BUILTIN.iter().map(|b| compile(b.def())).collect(), plugin: Vec::new(), active: Vec::new(), generic: compile(generic) };
        k.refresh();
        k
    }

    fn refresh(&mut self) {
        let shadowed = |b: &Arc<Kind>| self.plugin.iter().any(|p| p.def.name == b.def.name);
        self.active = self.plugin.iter().cloned().chain(self.builtin.iter().filter(|b| !shadowed(b)).cloned()).collect();
    }

    /// Active kinds in detection order.
    pub fn all(&self) -> &[Arc<Kind>] {
        &self.active
    }

    /// The kind of panes that report their status explicitly.
    pub fn generic(&self) -> Arc<Kind> {
        self.generic.clone()
    }

    pub fn by_name(&self, name: &str) -> Option<Arc<Kind>> {
        if name == GENERIC {
            return Some(self.generic());
        }
        self.active.iter().find(|k| k.def.name == name).cloned()
    }

    /// Whether `k` is still in use (not removed or replaced).
    pub fn current(&self, k: &Arc<Kind>) -> bool {
        Arc::ptr_eq(k, &self.generic) || self.active.iter().any(|a| Arc::ptr_eq(a, k))
    }

    /// The plugin that defines `name`, if one does.
    pub fn owner(&self, name: &str) -> Option<u32> {
        self.plugin.iter().find(|k| k.def.name == name).and_then(|k| k.plugin)
    }

    /// Whether a built-in kind is called `name`.
    pub fn is_builtin(&self, name: &str) -> bool {
        self.builtin.iter().any(|k| k.def.name == name)
    }

    /// Replace the kinds `plugin` defines (none: remove them).
    pub fn set_plugin(&mut self, plugin: u32, kinds: Vec<Kind>) {
        self.plugin.retain(|k| k.plugin != Some(plugin));
        self.plugin.extend(kinds.into_iter().map(|mut k| {
            k.plugin = Some(plugin);
            Arc::new(k)
        }));
        self.refresh();
    }

    /// Identify the agent running in a pane from its foreground command line.
    pub fn detect(&self, cmdline: &str) -> Option<Arc<Kind>> {
        if cmdline.is_empty() {
            return None;
        }
        let head = head(cmdline).join(" ");
        self.active.iter().find(|k| k.cmd.is_match(&head)).cloned()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum State {
    /// Needs the user (approval, question).
    Waiting,
    Working,
    /// Finished its turn; output not seen yet.
    Done,
    Idle,
    Exited,
}

impl State {
    pub fn label(&self) -> &'static str {
        match self {
            State::Waiting => "WAITING",
            State::Working => "WORKING",
            State::Done => "DONE",
            State::Idle => "IDLE",
            State::Exited => "EXITED",
        }
    }
    pub fn icon(&self) -> &'static str {
        match self {
            State::Waiting => "▲",
            State::Working => "●",
            State::Done => "✓",
            State::Idle => "○",
            State::Exited => "■",
        }
    }
    pub fn colour(&self) -> u8 {
        match self {
            State::Waiting => 196,
            State::Working => 214,
            State::Done => 42,
            State::Idle => 245,
            State::Exited => 240,
        }
    }
    pub fn parse(s: &str) -> Option<State> {
        Some(match s.to_ascii_lowercase().as_str() {
            "waiting" | "wait" | "input" | "approval" | "attention" => State::Waiting,
            "working" | "busy" | "running" => State::Working,
            "done" | "finished" | "complete" => State::Done,
            "idle" | "ready" => State::Idle,
            _ => return None,
        })
    }
}

/// Per-pane agent tracking.
#[derive(Clone, Debug)]
pub struct Tracker {
    pub agent: Option<Arc<Kind>>,
    pub state: State,
    pub summary: String,
    /// What the agent is working on (its terminal title, when meaningful).
    pub topic: String,
    pub since: Instant,
    pub explicit: Option<(State, String, Instant)>,
    /// The process group of the program in the pane that set the explicit
    /// status, if one did: the mark lasts while it is in the foreground.
    pub mark_pg: Option<u32>,
    pub progress: Option<(u8, u8, Instant)>,
    pub last_output: Option<Instant>,
    pub notified: Option<(String, Instant)>,
    pub bell: bool,
    /// Waiting-prompt lines the user already answered (stale until they leave the screen).
    pub answered: Vec<String>,
    /// Output activity samples (1 per second bucket) for the sparkline.
    pub activity: Vec<u16>,
    pub activity_epoch: u64,
}

impl Default for Tracker {
    fn default() -> Self {
        Tracker {
            agent: None,
            state: State::Idle,
            summary: String::new(),
            topic: String::new(),
            since: Instant::now(),
            explicit: None,
            mark_pg: None,
            progress: None,
            last_output: None,
            notified: None,
            bell: false,
            answered: Vec::new(),
            activity: vec![0; 60],
            activity_epoch: 0,
        }
    }
}

const WORKING_LINGER: Duration = Duration::from_millis(2500);

fn is_box_char(c: char) -> bool {
    ('\u{2500}'..='\u{259f}').contains(&c)
}

/// A border or horizontal rule (input boxes, separators).
fn is_rule(s: &str) -> bool {
    let (mut boxc, mut other) = (0usize, 0usize);
    for c in s.chars().filter(|c| !c.is_whitespace()) {
        if is_box_char(c) { boxc += 1 } else { other += 1 }
    }
    boxc >= 3 && boxc >= other * 2
}

fn clean_line(s: &str) -> String {
    let t: String = s.chars().map(|c| if is_box_char(c) { ' ' } else { c }).collect::<String>();
    // Drop leading bullets, prompts and spinner glyphs.
    let t = t.trim_start_matches(|c: char| !c.is_alphanumeric() && !"([{\"'`~/.".contains(c)).trim();
    let mut out = String::with_capacity(t.len());
    let mut sp = false;
    for c in t.chars() {
        if c.is_whitespace() {
            if !sp {
                out.push(' ');
            }
            sp = true;
        } else {
            out.push(c);
            sp = false;
        }
    }
    out.chars().take(160).collect()
}

impl Tracker {
    pub fn note_output(&mut self, now: Instant, bytes: usize, epoch_sec: u64) {
        self.last_output = Some(now);
        self.bump_activity(epoch_sec, bytes);
    }

    fn bump_activity(&mut self, epoch_sec: u64, bytes: usize) {
        if self.activity_epoch == 0 {
            self.activity_epoch = epoch_sec;
        }
        if epoch_sec > self.activity_epoch {
            let shift = (epoch_sec - self.activity_epoch).min(60) as usize;
            self.activity.drain(..shift);
            self.activity.extend(std::iter::repeat_n(0, shift));
            self.activity_epoch = epoch_sec;
        }
        if let Some(last) = self.activity.last_mut() {
            *last = last.saturating_add(bytes.min(u16::MAX as usize) as u16);
        }
    }

    /// Recompute state from signals. `lines` are the visible screen rows
    /// (bottom ones matter most). Returns true if state changed.
    pub fn update(&mut self, now: Instant, lines: &[String], title: &str, exited: bool, epoch_sec: u64) -> bool {
        self.bump_activity(epoch_sec, 0);
        let Some(kind) = self.agent.clone() else {
            return false;
        };
        let c: &Kind = &kind;
        let prev = self.state;
        let tail: Vec<&String> = lines.iter().rev().filter(|l| !l.trim().is_empty()).take(12).collect();
        // Bottom-up: the most recent signal on screen wins.
        let mut found: Option<(State, String)> = None;
        for (k, l) in tail.iter().enumerate() {
            if c.waiting.iter().any(|r| r.is_match(l)) {
                if self.answered.iter().any(|a| a == l.trim()) {
                    continue;
                }
                // Prefer the question above an option list ("❯ 1. Yes").
                let mut summary = clean_line(l);
                if !l.contains('?') {
                    for q in tail.iter().skip(k + 1).take(4) {
                        if q.contains('?') {
                            summary = clean_line(q);
                            break;
                        }
                    }
                }
                found = Some((State::Waiting, summary));
                break;
            }
            if c.working.iter().any(|r| r.is_match(l)) {
                found = Some((State::Working, clean_line(l)));
                break;
            }
        }
        if !self.answered.is_empty() && !tail.iter().any(|l| self.answered.iter().any(|a| a == l.trim())) {
            self.answered.clear();
        }
        let recent_output = self.last_output.is_some_and(|t| now.duration_since(t) < WORKING_LINGER);
        let progress_on = self.progress.is_some_and(|(s, _, t)| (s == 1 || s == 3) && now.duration_since(t) < Duration::from_secs(60));
        let (state, summary) = if exited {
            (State::Exited, "process exited".to_string())
        } else if let Some((st, m, _)) = &self.explicit {
            (*st, if m.is_empty() { self.best_summary(lines, c, title) } else { m.clone() })
        } else if let Some((st, m)) = found {
            (st, m)
        } else if progress_on || recent_output && (prev == State::Working || matches!(prev, State::Idle | State::Done) && self.activity_burst()) {
            (State::Working, self.best_summary(lines, c, title))
        } else {
            let s = if matches!(prev, State::Working | State::Waiting | State::Done) { State::Done } else { State::Idle };
            (s, self.best_summary(lines, c, title))
        };
        self.summary = summary;
        self.topic = topic_of(title, c.name());
        if state != prev {
            self.state = state;
            self.since = now;
            return true;
        }
        false
    }

    /// Sustained output over the last few seconds (spinners, streaming).
    fn activity_burst(&self) -> bool {
        let n = self.activity.len();
        n >= 3 && self.activity[n - 3..].iter().filter(|&&b| b > 0).count() >= 2
    }

    /// Latest meaningful output line, else a notification, else the title.
    fn best_summary(&self, lines: &[String], c: &Kind, title: &str) -> String {
        if let Some(l) = last_output_line(lines, &c.chrome) {
            return l;
        }
        if let Some((n, _)) = &self.notified
            && !n.is_empty()
        {
            return n.clone();
        }
        clean_line(title)
    }

    /// The user typed into the pane: an open question is now answered.
    pub fn note_input(&mut self, now: Instant, lines: &[String]) {
        if self.state == State::Waiting && self.explicit.is_none() {
            if let Some(c) = self.agent.clone() {
                self.answered = lines
                    .iter()
                    .rev()
                    .filter(|l| !l.trim().is_empty())
                    .take(12)
                    .filter(|l| c.waiting.iter().any(|r| r.is_match(l)))
                    .map(|l| l.trim().to_string())
                    .collect();
            }
            self.state = State::Working;
            self.since = now;
        }
        if let Some((State::Waiting, _, _)) = &self.explicit {
            self.explicit = None;
        }
    }

    /// The user looked at the pane: completed work is acknowledged.
    pub fn seen(&mut self) {
        self.bell = false;
        self.notified = None;
        if self.state == State::Done {
            self.state = State::Idle;
        }
        // An explicit "done" is acknowledged too (otherwise it would be
        // re-asserted on the next update).
        if matches!(self.explicit, Some((State::Done, _, _))) {
            self.explicit = None;
        }
    }

    pub fn needs_attention(&self) -> bool {
        matches!(self.state, State::Waiting | State::Done) || self.bell
    }

    pub fn sparkline(&self, width: usize) -> String {
        const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        let n = self.activity.len();
        let take = width.min(n);
        let slice = &self.activity[n - take..];
        let max = slice.iter().copied().max().unwrap_or(0).max(1) as f32;
        slice.iter().map(|&v| if v == 0 { ' ' } else { BARS[((v as f32 / max) * 7.0).round() as usize] }).collect()
    }
}

/// The most recent line of real output: bottom-up, skipping an input box or
/// rule at the bottom of the screen, UI chrome and short fragments.
fn last_output_line(lines: &[String], chrome: &[Regex]) -> Option<String> {
    let rows: Vec<&String> = lines.iter().rev().filter(|l| !l.trim().is_empty()).take(30).collect();
    // Everything below the topmost border in the bottom few rows is input UI.
    let start = (0..rows.len().min(8)).rev().find(|&k| is_rule(rows[k])).map(|k| k + 1).unwrap_or(0);
    rows[start..]
        .iter()
        .filter(|l| !is_rule(l) && !chrome.iter().any(|r| r.is_match(l)))
        .map(|l| clean_line(l))
        .find(|l| l.chars().filter(|ch| ch.is_alphanumeric()).count() >= 3)
}

/// A terminal title worth showing as the agent's topic.
fn topic_of(title: &str, agent: &str) -> String {
    let t = clean_line(title);
    let squashed: String = t.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_lowercase();
    let generic = squashed.starts_with(agent) && squashed.len() <= agent.len() + 6;
    let shellish = t.starts_with('/') || t.starts_with('~') || t.split_whitespace().next().is_some_and(|w| w.contains('@') && w.ends_with(':'));
    if generic || shellish || !t.chars().any(|c| c.is_alphanumeric()) { String::new() } else { t }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detect(cmdline: &str) -> Option<Arc<Kind>> {
        Kinds::new().detect(cmdline)
    }

    fn name_of(cmdline: &str) -> Option<String> {
        detect(cmdline).map(|k| k.def.name.clone())
    }

    #[test]
    fn detects_agents_from_cmdline() {
        assert_eq!(name_of("node /home/u/.asdf/installs/nodejs/v22/bin/claude --resume").as_deref(), Some("claude"));
        assert_eq!(name_of("/home/u/.local/bin/pi").as_deref(), Some("pi"));
        assert_eq!(name_of("pip install foo"), None);
        assert_eq!(name_of("bash"), None);
        assert_eq!(name_of("codex exec hi").as_deref(), Some("codex"));
        assert_eq!(name_of("node /usr/lib/node_modules/@github/copilot/index.js").as_deref(), Some("copilot"));
        assert_eq!(name_of("vim claude.md"), None);
        assert_eq!(name_of("/bin/sh -c /tmp/x/claude --resume").as_deref(), Some("claude"));
        assert_eq!(name_of("sh -c vim claude.md"), None);
        assert_eq!(program("/bin/sh -c /opt/bin/kiro --x"), "kiro");
        assert_eq!(program(""), "");
        assert!(runs("sh -c /tmp/x/kiro", "kiro") && runs("node /usr/lib/kiro/kiro --resume", "kiro"));
        assert!(!runs("node /usr/share/claude/cli.js", "sh") && !runs("kiro", ""));
    }

    fn plugin_def(name: &str, cmd: &str) -> Def {
        Def { name: name.into(), cmd: cmd.into(), ..Default::default() }
    }

    #[test]
    fn plugins_add_and_replace_definitions() {
        let mut kinds = Kinds::new();
        let builtin_claude = kinds.by_name("claude").unwrap();
        assert!(kinds.detect("kiro-cli chat").is_none());
        let kiro = Def {
            waiting: vec![r"allow this action".into()],
            resume: Some("kiro-cli chat --resume".into()),
            ..plugin_def("kiro", r"(^|[/ ])kiro(-cli)?( |$)")
        };
        let mine = Def { waiting: vec![r"new prompt".into()], ..plugin_def("claude", r"(^|[/ ])claude( |$)") };
        kinds.set_plugin(7, vec![Kind::compile(kiro, Some(7)).unwrap(), Kind::compile(mine, Some(7)).unwrap()]);
        let k = kinds.detect("/usr/bin/kiro-cli chat").unwrap();
        assert_eq!((k.def.name.as_str(), k.plugin), ("kiro", Some(7)));
        assert_eq!(kinds.owner("kiro"), Some(7));
        // A plugin definition replaces the built-in one of the same name.
        let c = kinds.detect("claude").unwrap();
        assert_eq!(c.plugin, Some(7));
        assert!(!kinds.current(&builtin_claude) && kinds.current(&c));
        assert_eq!(kinds.all().iter().filter(|k| k.def.name == "claude").count(), 1);
        // Its patterns apply, plus the shared ones.
        let mut t = Tracker { agent: Some(c), ..Default::default() };
        t.update(Instant::now(), &lines(&["new prompt here"]), "", false, 1);
        assert_eq!(t.state, State::Waiting);
        // Unloading the plugin brings the built-in definitions back.
        kinds.set_plugin(7, Vec::new());
        assert!(kinds.detect("kiro").is_none());
        assert_eq!(kinds.detect("claude").unwrap().plugin, None);
        assert!(kinds.is_builtin("claude") && !kinds.is_builtin("kiro"));
    }

    #[test]
    fn rejects_bad_definitions() {
        let bad = |d: Def| Kind::compile(d, Some(1)).unwrap_err();
        assert!(bad(plugin_def("my agent", "x")).contains("invalid name"));
        assert!(bad(plugin_def(GENERIC, "x")).contains("reserved"));
        assert!(bad(plugin_def("a", "")).contains("cmd"));
        assert!(bad(plugin_def("a", "(")).contains("cmd"));
        assert!(bad(plugin_def("a", ".*")).contains("every command"));
        assert!(bad(plugin_def("a", "sh")).contains("plain shell"));
        assert!(bad(Def { waiting: vec!["x?".into()], ..plugin_def("a", "agentx") }).contains("every line"));
        assert!(bad(Def { working: vec!["(".into()], ..plugin_def("a", "agentx") }).contains("working"));
        assert!(bad(Def { resume: Some("a\nrm -rf x".into()), ..plugin_def("a", "agentx") }).contains("resume"));
        assert!(Kind::compile(plugin_def("a", "agentx"), Some(1)).is_ok());
        let typo: Result<Def, _> = serde_json::from_str(r#"{"name":"a","cmd":"x","wait":["y"]}"#);
        assert!(typo.unwrap_err().to_string().contains("unknown field"));
    }

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn summary_skips_input_box_and_chrome() {
        let chrome = &detect("claude").unwrap().chrome;
        let claude = lines(&[
            "⏺ Fixed the off-by-one in parser.rs; all 42 tests pass.",
            "",
            "╭──────────────────────────────────────────╮",
            "│ >                                        │",
            "╰──────────────────────────────────────────╯",
            "  ? for shortcuts",
        ]);
        assert_eq!(last_output_line(&claude, chrome).as_deref(), Some("Fixed the off-by-one in parser.rs; all 42 tests pass."));
        let rules = lines(&["⏺ Done: README updated", "────────────────", "> ", "────────────────", "  ⏵⏵ accept edits on (shift+tab to cycle)"]);
        assert_eq!(last_output_line(&rules, chrome).as_deref(), Some("Done: README updated"));
        let codex = &detect("codex").unwrap().chrome;
        let hints = lines(&["• Added retry logic to fetch()", "▌ Ask Codex to do anything", " ⏎ send   ⌃J newline   ⌃C quit"]);
        assert_eq!(last_output_line(&hints, codex).as_deref(), Some("Added retry logic to fetch()"));
    }

    #[test]
    fn explicit_done_is_acknowledged_when_seen() {
        let mut t = Tracker { agent: Some(Kinds::new().generic()), ..Default::default() };
        let now = Instant::now();
        t.explicit = Some((State::Done, "built".into(), now));
        t.update(now, &[], "", false, 1);
        assert_eq!(t.state, State::Done);
        t.seen();
        t.update(now, &[], "", false, 1);
        assert_eq!(t.state, State::Idle);
        assert!(!t.needs_attention());
    }

    #[test]
    fn topic_ignores_generic_titles() {
        assert_eq!(topic_of("✳ Fix failing parser tests", "claude"), "Fix failing parser tests");
        assert_eq!(topic_of("✳ Claude Code", "claude"), "");
        assert_eq!(topic_of("user@host: ~/dev/project", "claude"), "");
        assert_eq!(topic_of("~/dev/mono", "codex"), "");
        assert_eq!(topic_of("codex", "codex"), "");
    }

    #[test]
    fn infers_states() {
        let mut t = Tracker { agent: detect("claude"), ..Default::default() };
        let now = Instant::now();
        t.note_output(now, 100, 1000);
        assert!(t.update(now, &lines(&["", "✻ Reading files… (12s · esc to interrupt)", "> "]), "", false, 1000));
        assert_eq!(t.state, State::Working);
        assert!(t.summary.contains("Reading files"), "{}", t.summary);
        t.update(now, &lines(&["Do you want to proceed?", "❯ 1. Yes", "  2. No"]), "", false, 1000);
        assert_eq!(t.state, State::Waiting);
        assert_eq!(t.summary, "Do you want to proceed?");
        // The user answers; the stale prompt stays on screen above new output.
        t.note_input(now, &lines(&["Do you want to proceed?", "❯ 1. Yes", "  2. No"]));
        t.update(now, &lines(&["Do you want to proceed?", "❯ 1. Yes", "  2. No", "Applying change...", "✻ Writing (1s · esc to interrupt)"]), "", false, 1000);
        assert_eq!(t.state, State::Working);
        // Still answered once the spinner is gone: finishes as Done, not Waiting.
        let quiet = now + Duration::from_secs(5);
        t.update(quiet, &lines(&["Do you want to proceed?", "❯ 1. Yes", "  2. No", "Applying change...", "All done.", "> "]), "", false, 1005);
        assert_eq!(t.state, State::Done);
        t.seen();
        // Approval given, work finished, no output for a while -> Done (unseen).
        t.update(now, &lines(&["", "✻ Writing (3s · esc to interrupt)"]), "", false, 1000);
        let later = now + Duration::from_secs(10);
        t.update(later, &lines(&["All tests pass.", "> "]), "", false, 1010);
        assert_eq!(t.state, State::Done);
        assert!(t.needs_attention());
        t.seen();
        assert_eq!(t.state, State::Idle);
        t.update(later, &lines(&[]), "", true, 1010);
        assert_eq!(t.state, State::Exited);
    }

    #[test]
    fn explicit_status_wins() {
        let mut t = Tracker { agent: detect("codex"), ..Default::default() };
        t.explicit = Some((State::Waiting, "needs review".into(), Instant::now()));
        t.update(Instant::now(), &lines(&["esc to interrupt"]), "", false, 5);
        assert_eq!(t.state, State::Waiting);
        assert_eq!(t.summary, "needs review");
    }

    #[test]
    fn sparkline_shape() {
        let mut t = Tracker::default();
        t.note_output(Instant::now(), 10, 100);
        t.note_output(Instant::now(), 80, 101);
        let s = t.sparkline(4);
        assert_eq!(s.chars().count(), 4);
        assert!(s.ends_with('█'));
    }
}
