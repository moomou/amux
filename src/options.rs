//! Option store with tmux defaults. Scopes: global, per-session, per-window.

use std::collections::HashMap;

pub fn defaults() -> Vec<(&'static str, String)> {
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into());
    let editor = format!("{}{}", std::env::var("VISUAL").unwrap_or_default(), std::env::var("EDITOR").unwrap_or_default());
    let mode_keys = if editor.contains("vi") { "vi" } else { "emacs" };
    let s = |v: &str| v.to_string();
    vec![
        ("prefix", s("C-b")),
        ("prefix2", s("None")),
        ("mouse", s("off")),
        ("mode-keys", s(mode_keys)),
        ("status-keys", s("emacs")),
        ("status", s("on")),
        ("status-position", s("bottom")),
        ("status-interval", s("15")),
        ("status-justify", s("left")),
        ("status-style", s("bg=green,fg=black")),
        ("status-left", s("[#{session_name}] ")),
        ("status-left-length", s("10")),
        ("status-left-style", s("default")),
        ("status-right", s("#{amux_agents}\"#{=21:pane_title}\" %H:%M %d-%b-%y")),
        ("status-right-length", s("40")),
        ("status-right-style", s("default")),
        ("window-status-format", s("#I:#W#{?window_flags,#{window_flags}, }")),
        ("window-status-current-format", s("#I:#W#{?window_flags,#{window_flags}, }")),
        ("window-status-separator", s(" ")),
        ("window-status-style", s("default")),
        ("window-status-current-style", s("default")),
        ("window-status-last-style", s("default")),
        ("window-status-bell-style", s("reverse")),
        ("window-status-activity-style", s("reverse")),
        ("message-style", s("bg=yellow,fg=black")),
        ("message-command-style", s("bg=black,fg=yellow")),
        ("mode-style", s("bg=yellow,fg=black")),
        ("pane-border-style", s("default")),
        ("pane-active-border-style", s("fg=green")),
        ("display-panes-colour", s("blue")),
        ("display-panes-active-colour", s("red")),
        ("display-panes-time", s("1000")),
        ("display-time", s("750")),
        ("history-limit", s("2000")),
        ("base-index", s("0")),
        ("pane-base-index", s("0")),
        ("renumber-windows", s("off")),
        ("escape-time", s("500")),
        ("repeat-time", s("500")),
        ("default-shell", shell),
        ("default-command", s("")),
        ("default-terminal", s("tmux-256color")),
        ("allow-rename", s("off")),
        ("automatic-rename", s("on")),
        ("automatic-rename-format", s("#{?pane_in_mode,[amux],#{pane_current_command}}#{?pane_dead,[dead],}")),
        ("remain-on-exit", s("off")),
        ("set-titles", s("off")),
        ("set-titles-string", s("#S:#I:#W - \"#T\"")),
        ("synchronize-panes", s("off")),
        ("aggressive-resize", s("off")),
        ("focus-events", s("off")),
        ("set-clipboard", s("external")),
        ("word-separators", s(" ")),
        ("wrap-search", s("on")),
        ("main-pane-width", s("80")),
        ("main-pane-height", s("24")),
        ("monitor-activity", s("off")),
        ("monitor-bell", s("on")),
        ("bell-action", s("any")),
        ("visual-bell", s("off")),
        ("visual-activity", s("off")),
        ("destroy-unattached", s("off")),
        ("exit-empty", s("on")),
        ("exit-unattached", s("off")),
        ("xterm-keys", s("on")),
        ("update-environment", s("DISPLAY KRB5CCNAME SSH_ASKPASS SSH_AUTH_SOCK SSH_AGENT_PID SSH_CONNECTION WINDOWID XAUTHORITY")),
        ("default-size", s("80x24")),
        ("window-size", s("latest")),
        ("lock-after-time", s("0")),
        // amux
        ("agent-badge", s("on")),
        ("agent-resume", s("typed")),
        ("snapshot-interval", s("30")),
        ("tmux-env", s("off")),
        ("plugins", s("on")),
    ]
}

#[derive(Default)]
pub struct Options {
    pub global: HashMap<String, String>,
    pub session: HashMap<u32, HashMap<String, String>>,
    pub window: HashMap<u32, HashMap<String, String>>,
}

pub struct Scope {
    pub session: Option<u32>,
    pub window: Option<u32>,
}

impl Options {
    pub fn new() -> Options {
        let mut o = Options::default();
        for (k, v) in defaults() {
            o.global.insert(k.to_string(), v);
        }
        o
    }

    pub fn get(&self, sc: &Scope, key: &str) -> String {
        if let Some(w) = sc.window
            && let Some(v) = self.window.get(&w).and_then(|m| m.get(key))
        {
            return v.clone();
        }
        if let Some(s) = sc.session
            && let Some(v) = self.session.get(&s).and_then(|m| m.get(key))
        {
            return v.clone();
        }
        self.global.get(key).cloned().unwrap_or_default()
    }

    pub fn flag(&self, sc: &Scope, key: &str) -> bool {
        matches!(self.get(sc, key).as_str(), "on" | "1" | "yes" | "true")
    }

    pub fn num(&self, sc: &Scope, key: &str, def: i64) -> i64 {
        self.get(sc, key).trim().parse().unwrap_or(def)
    }

    pub fn is_known(key: &str) -> bool {
        key.starts_with('@') || defaults().iter().any(|(k, _)| *k == key) || KNOWN_EXTRA.contains(&key)
    }
}

/// Options we accept (and store) but do not act on.
const KNOWN_EXTRA: &[&str] = &[
    "terminal-overrides",
    "terminal-features",
    "command-alias",
    "buffer-limit",
    "message-limit",
    "copy-command",
    "extended-keys",
    "status-bg",
    "status-fg",
    "pane-border-status",
    "pane-border-format",
    "pane-border-lines",
    "pane-border-indicators",
    "mode-mouse",
    "mouse-select-pane",
    "mouse-resize-pane",
    "mouse-select-window",
    "utf8",
    "status-utf8",
    "visual-silence",
    "monitor-silence",
    "activity-action",
    "silence-action",
    "clock-mode-colour",
    "clock-mode-style",
    "window-active-style",
    "window-style",
    "popup-style",
    "popup-border-style",
    "other-pane-width",
    "other-pane-height",
    "alternate-screen",
    "assume-paste-time",
    "detach-on-destroy",
    "key-table",
    "status-format",
    "window-status-current-bg",
    "window-status-current-fg",
    "window-status-bg",
    "window-status-fg",
    "lock-command",
    "default-path",
    "allow-passthrough",
    "scroll-on-clear",
    "copy-mode-match-style",
    "copy-mode-current-match-style",
    "copy-mode-mark-style",
    "fill-character",
    "editor",
    "focus-follows-mouse",
    "prompt-history-limit",
    "get-clipboard",
    "backspace",
    "remain-on-exit-format",
];
