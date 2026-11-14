//! tmux command language: tokenizer (quotes, escapes, `\;`, `{}` blocks,
//! `~`/`$VAR` expansion, comments, line continuation, `%if`) and
//! getopt-style argument parsing.

use std::collections::HashMap;

pub type Argv = Vec<String>;

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    Sep,
    Newline,
}

/// Tokenize command text. `env` resolves `$VAR`.
fn tokenize(src: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<Vec<Tok>, String> {
    let mut out = Vec::new();
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut word = String::new();
    let mut in_word = false;
    let push_word = |word: &mut String, in_word: &mut bool, out: &mut Vec<Tok>| {
        if *in_word {
            out.push(Tok::Word(std::mem::take(word)));
            *in_word = false;
        }
    };
    let expand_var = |chars: &[char], i: &mut usize, word: &mut String| {
        // at chars[*i] == '$'
        let mut j = *i + 1;
        let name: String;
        if j < chars.len() && chars[j] == '{' {
            let start = j + 1;
            while j < chars.len() && chars[j] != '}' {
                j += 1;
            }
            name = chars[start..j.min(chars.len())].iter().collect();
            j += 1;
        } else {
            let start = j;
            while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
                j += 1;
            }
            name = chars[start..j].iter().collect();
        }
        if name.is_empty() {
            word.push('$');
            *i += 1;
            return;
        }
        if let Some(v) = env(&name) {
            word.push_str(&v);
        }
        *i = j;
    };
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' | '\r' => {
                push_word(&mut word, &mut in_word, &mut out);
                i += 1;
            }
            '\n' => {
                push_word(&mut word, &mut in_word, &mut out);
                out.push(Tok::Newline);
                i += 1;
            }
            ';' => {
                push_word(&mut word, &mut in_word, &mut out);
                out.push(Tok::Sep);
                i += 1;
            }
            '#' if !in_word && chars.get(i + 1) != Some(&'{') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '\\' => {
                i += 1;
                match chars.get(i) {
                    Some('\n') => i += 1,
                    Some(';') => {
                        word.push(';');
                        in_word = true;
                        i += 1;
                    }
                    Some(&n) => {
                        word.push(n);
                        in_word = true;
                        i += 1;
                    }
                    None => {}
                }
            }
            '\'' => {
                in_word = true;
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    word.push(chars[i]);
                    i += 1;
                }
                if i >= chars.len() {
                    return Err("unterminated single quote".into());
                }
                i += 1;
            }
            '"' => {
                in_word = true;
                i += 1;
                loop {
                    if i >= chars.len() {
                        return Err("unterminated double quote".into());
                    }
                    match chars[i] {
                        '"' => {
                            i += 1;
                            break;
                        }
                        '\\' => {
                            i += 1;
                            match chars.get(i) {
                                Some('n') => word.push('\n'),
                                Some('t') => word.push('\t'),
                                Some('r') => word.push('\r'),
                                Some('e') => word.push('\x1b'),
                                Some('\n') | None => {}
                                Some('u') => {
                                    let hex: String = chars[i + 1..(i + 5).min(chars.len())].iter().collect();
                                    if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                                        word.push(ch);
                                        i += 4;
                                    } else {
                                        word.push('u');
                                    }
                                }
                                Some(&n) => word.push(n),
                            }
                            i += 1;
                        }
                        '$' => expand_var(&chars, &mut i, &mut word),
                        ch => {
                            word.push(ch);
                            i += 1;
                        }
                    }
                }
            }
            '{' if !in_word => {
                // command block: capture raw text up to the matching brace
                let mut depth = 1;
                let start = i + 1;
                let mut j = start;
                let mut q: Option<char> = None;
                while j < chars.len() && depth > 0 {
                    let ch = chars[j];
                    match q {
                        Some(qc) => {
                            if ch == '\\' && qc == '"' {
                                j += 1;
                            } else if ch == qc {
                                q = None;
                            }
                        }
                        None => match ch {
                            '\'' | '"' => q = Some(ch),
                            '{' => depth += 1,
                            '}' => depth -= 1,
                            '\\' => j += 1,
                            _ => {}
                        },
                    }
                    j += 1;
                }
                if depth != 0 {
                    return Err("unterminated brace".into());
                }
                let body: String = chars[start..j - 1].iter().collect();
                out.push(Tok::Word(body.trim().to_string()));
                i = j;
            }
            '~' if !in_word && (chars.get(i + 1).is_none_or(|&n| n == '/' || n.is_whitespace() || n == ';')) => {
                word.push_str(&crate::util::home());
                in_word = true;
                i += 1;
            }
            '$' => {
                in_word = true;
                expand_var(&chars, &mut i, &mut word);
            }
            _ => {
                word.push(c);
                in_word = true;
                i += 1;
            }
        }
    }
    push_word(&mut word, &mut in_word, &mut out);
    Ok(out)
}

/// One parsed line may contain several commands separated by `;`.
pub type CmdList = Vec<Argv>;

/// Parse a command string (single logical unit, e.g. from the prompt or `bind`).
pub fn parse_cmdlist(src: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<CmdList, String> {
    let toks = tokenize(src, env)?;
    let mut list = Vec::new();
    let mut cur = Vec::new();
    for t in toks {
        match t {
            Tok::Word(w) => cur.push(w),
            Tok::Sep | Tok::Newline => {
                if !cur.is_empty() {
                    list.push(std::mem::take(&mut cur));
                }
            }
        }
    }
    if !cur.is_empty() {
        list.push(cur);
    }
    Ok(list)
}

/// Split an argv at literal ";" arguments (from `\;`) into a command list.
pub fn split_semicolons(args: &[String]) -> CmdList {
    let mut list = Vec::new();
    let mut cur: Argv = Vec::new();
    for a in args {
        if a == ";" {
            if !cur.is_empty() {
                list.push(std::mem::take(&mut cur));
            }
        } else if let Some(stripped) = a.strip_suffix(';')
            && !stripped.ends_with('\\')
            && !stripped.is_empty()
            && cur.is_empty()
        {
            cur.push(stripped.to_string());
            list.push(std::mem::take(&mut cur));
        } else {
            cur.push(a.clone());
        }
    }
    if !cur.is_empty() {
        list.push(cur);
    }
    list
}

pub struct ConfigLine {
    pub line: usize,
    pub cmds: CmdList,
}

/// Parse a config file into lines of commands, evaluating `%if` blocks with
/// `cond` and handling `NAME=value` assignments via `assign`.
pub fn parse_config(src: &str, env: &dyn Fn(&str) -> Option<String>, cond: &mut dyn FnMut(&str) -> bool) -> (Vec<ConfigLine>, Vec<String>) {
    let mut out = Vec::new();
    let mut errors = Vec::new();
    // Join continuation lines and brace blocks into logical lines first.
    let mut logical: Vec<(usize, String)> = Vec::new();
    let mut acc = String::new();
    let mut start_line = 0;
    let mut depth: i32 = 0;
    for (n, raw) in src.lines().enumerate() {
        if acc.is_empty() {
            start_line = n + 1;
        }
        let cont = raw.ends_with('\\') && !raw.ends_with("\\\\");
        let piece = if cont { &raw[..raw.len() - 1] } else { raw };
        acc.push_str(piece);
        depth += brace_delta(piece);
        if cont {
            continue;
        }
        if depth > 0 {
            acc.push('\n');
            continue;
        }
        depth = 0;
        logical.push((start_line, std::mem::take(&mut acc)));
    }
    if !acc.is_empty() {
        logical.push((start_line, acc));
    }
    // %if handling: stack of (taking, any_taken)
    let mut stack: Vec<(bool, bool)> = Vec::new();
    for (ln, text) in logical {
        let t = text.trim_start();
        if let Some(rest) = t.strip_prefix("%if") {
            let parent = stack.iter().all(|s| s.0);
            let c = parent && cond(&unquote(rest.trim()));
            stack.push((c, c));
            continue;
        }
        if let Some(rest) = t.strip_prefix("%elif") {
            let parent = stack.len() <= 1 || stack[..stack.len() - 1].iter().all(|s| s.0);
            if let Some(top) = stack.last_mut() {
                if top.1 {
                    top.0 = false;
                } else {
                    let c = parent && cond(&unquote(rest.trim()));
                    top.0 = c;
                    top.1 = c;
                }
            }
            continue;
        }
        if t.starts_with("%else") {
            if let Some(top) = stack.last_mut() {
                top.0 = !top.1;
                top.1 = true;
            }
            continue;
        }
        if t.starts_with("%endif") {
            stack.pop();
            continue;
        }
        if !stack.iter().all(|s| s.0) {
            continue;
        }
        let t = t.strip_prefix("%hidden").map(|r| r.trim_start()).unwrap_or(t);
        match parse_cmdlist(t, env) {
            Ok(cmds) if !cmds.is_empty() => out.push(ConfigLine { line: ln, cmds }),
            Ok(_) => {}
            Err(e) => errors.push(format!("line {}: {}", ln, e)),
        }
    }
    (out, errors)
}

fn brace_delta(s: &str) -> i32 {
    let mut d = 0;
    let mut q: Option<char> = None;
    let mut prev = ' ';
    for c in s.chars() {
        match q {
            Some(qc) => {
                if c == qc && prev != '\\' {
                    q = None;
                }
            }
            None => match c {
                '\'' | '"' if prev != '\\' => q = Some(c),
                '#' if prev == ' ' || prev == '\t' => break,
                '{' if prev != '\\' && prev != '#' => d += 1,
                '}' if prev != '\\' => d -= 1,
                _ => {}
            },
        }
        prev = c;
    }
    d
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\''))) {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Parsed getopt-style arguments.
#[derive(Debug, Default, Clone)]
pub struct Args {
    pub flags: HashMap<char, Vec<String>>,
    pub pos: Vec<String>,
}

impl Args {
    /// `Args::parse`, but options may also follow positional arguments
    /// (`plugin new NAME -l sh`), the way people and agents tend to write them.
    pub fn parse_anywhere(args: &[String], spec: &str) -> Result<Args, String> {
        let (mut opts, mut pos) = (Vec::new(), Vec::new());
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            if a == "--" {
                pos.extend(args[i + 1..].iter().cloned());
                break;
            }
            let flag = a.strip_prefix('-').and_then(|r| r.chars().next()).filter(|f| *f != ':' && spec.contains(*f));
            if let Some(f) = flag {
                opts.push(a.clone());
                let takes = spec.split(f).nth(1).is_some_and(|r| r.starts_with(':'));
                if takes && a.len() == 2 && i + 1 < args.len() {
                    i += 1;
                    opts.push(args[i].clone());
                }
            } else {
                pos.push(a.clone());
            }
            i += 1;
        }
        opts.push("--".into());
        opts.extend(pos);
        Args::parse(&opts, spec)
    }

    /// `spec` uses getopt syntax: letters, `:` after letters that take values.
    pub fn parse(argv: &[String], spec: &str) -> Result<Args, String> {
        let mut takes = HashMap::new();
        let sc: Vec<char> = spec.chars().collect();
        for (i, &c) in sc.iter().enumerate() {
            if c != ':' {
                takes.insert(c, sc.get(i + 1) == Some(&':'));
            }
        }
        let mut a = Args::default();
        let mut i = 0;
        while i < argv.len() {
            let s = &argv[i];
            if s == "--" {
                a.pos.extend(argv[i + 1..].iter().cloned());
                break;
            }
            if s.len() > 1 && s.starts_with('-') && a.pos.is_empty() {
                let cs: Vec<char> = s[1..].chars().collect();
                let mut j = 0;
                while j < cs.len() {
                    let f = cs[j];
                    match takes.get(&f) {
                        None => return Err(format!("unknown flag -{}", f)),
                        Some(true) => {
                            let val = if j + 1 < cs.len() {
                                cs[j + 1..].iter().collect::<String>()
                            } else {
                                i += 1;
                                argv.get(i).cloned().ok_or_else(|| format!("-{} expects an argument", f))?
                            };
                            a.flags.entry(f).or_default().push(val);
                            break;
                        }
                        Some(false) => {
                            a.flags.entry(f).or_default().push(String::new());
                        }
                    }
                    j += 1;
                }
                i += 1;
                continue;
            }
            a.pos.push(s.clone());
            i += 1;
        }
        Ok(a)
    }

    pub fn has(&self, f: char) -> bool {
        self.flags.contains_key(&f)
    }
    pub fn get(&self, f: char) -> Option<&str> {
        self.flags.get(&f).and_then(|v| v.last()).map(|s| s.as_str())
    }
    pub fn count(&self, f: char) -> usize {
        self.flags.get(&f).map(|v| v.len()).unwrap_or(0)
    }
}

/// Canonical command name for a name or alias (with unique-prefix matching).
pub fn resolve_command(name: &str) -> Option<&'static str> {
    const ALIASES: &[(&str, &str)] = &[
        ("attach", "attach-session"),
        ("a", "attach-session"),
        ("at", "attach-session"),
        ("bind", "bind-key"),
        ("unbind", "unbind-key"),
        ("breakp", "break-pane"),
        ("capturep", "capture-pane"),
        ("clearhist", "clear-history"),
        ("confirm", "confirm-before"),
        ("detach", "detach-client"),
        ("display", "display-message"),
        ("displayp", "display-panes"),
        ("has", "has-session"),
        ("if", "if-shell"),
        ("joinp", "join-pane"),
        ("killp", "kill-pane"),
        ("killw", "kill-window"),
        ("last", "last-window"),
        ("lastp", "last-pane"),
        ("lsb", "list-buffers"),
        ("lsc", "list-clients"),
        ("lscm", "list-commands"),
        ("lsk", "list-keys"),
        ("ls", "list-sessions"),
        ("lsp", "list-panes"),
        ("lsw", "list-windows"),
        ("loadb", "load-buffer"),
        ("movew", "move-window"),
        ("new", "new-session"),
        ("neww", "new-window"),
        ("next", "next-window"),
        ("nextl", "next-layout"),
        ("pasteb", "paste-buffer"),
        ("prev", "previous-window"),
        ("prevl", "previous-layout"),
        ("refresh", "refresh-client"),
        ("rename", "rename-session"),
        ("renamew", "rename-window"),
        ("resizep", "resize-pane"),
        ("resizew", "resize-window"),
        ("respawnp", "respawn-pane"),
        ("respawnw", "respawn-window"),
        ("rotatew", "rotate-window"),
        ("run", "run-shell"),
        ("saveb", "save-buffer"),
        ("selectl", "select-layout"),
        ("selectp", "select-pane"),
        ("selectw", "select-window"),
        ("send", "send-keys"),
        ("set", "set-option"),
        ("setb", "set-buffer"),
        ("setenv", "set-environment"),
        ("setw", "set-window-option"),
        ("show", "show-options"),
        ("showb", "show-buffer"),
        ("showenv", "show-environment"),
        ("showmsgs", "show-messages"),
        ("showw", "show-window-options"),
        ("source", "source-file"),
        ("splitw", "split-window"),
        ("start", "start-server"),
        ("suspendc", "suspend-client"),
        ("swapp", "swap-pane"),
        ("swapw", "swap-window"),
        ("switchc", "switch-client"),
        ("deleteb", "delete-buffer"),
        ("choose-session", "choose-tree"),
        ("choose-window", "choose-tree"),
        ("wait", "wait-for"),
    ];
    const COMMANDS: &[&str] = &[
        "attach-session",
        "bind-key",
        "break-pane",
        "capture-pane",
        "choose-buffer",
        "choose-client",
        "choose-tree",
        "clear-history",
        "clock-mode",
        "command-prompt",
        "confirm-before",
        "copy-mode",
        "delete-buffer",
        "detach-client",
        "display-message",
        "display-panes",
        "has-session",
        "if-shell",
        "join-pane",
        "kill-pane",
        "kill-server",
        "kill-session",
        "kill-window",
        "last-pane",
        "last-window",
        "link-window",
        "list-buffers",
        "list-clients",
        "list-commands",
        "list-keys",
        "list-panes",
        "list-sessions",
        "list-windows",
        "load-buffer",
        "lock-server",
        "move-pane",
        "move-window",
        "new-session",
        "new-window",
        "next-layout",
        "next-window",
        "paste-buffer",
        "pipe-pane",
        "previous-layout",
        "previous-window",
        "refresh-client",
        "rename-session",
        "rename-window",
        "resize-pane",
        "resize-window",
        "respawn-pane",
        "respawn-window",
        "rotate-window",
        "run-shell",
        "save-buffer",
        "select-layout",
        "select-pane",
        "select-window",
        "send-keys",
        "send-prefix",
        "server-access",
        "set-buffer",
        "set-environment",
        "set-hook",
        "set-option",
        "set-window-option",
        "show-buffer",
        "show-environment",
        "show-hooks",
        "show-messages",
        "show-options",
        "show-window-options",
        "source-file",
        "split-window",
        "start-server",
        "suspend-client",
        "swap-pane",
        "swap-window",
        "switch-client",
        "unbind-key",
        "unlink-window",
        "wait-for",
        // amux extensions
        "agent-view",
        "agent-next",
        "agent-status",
        "agents",
        "panic-test",
        "snapshot-panes",
        "server-info",
        "plugin",
        "__migrate-import",
    ];
    if let Some(&(_, c)) = ALIASES.iter().find(|(a, _)| *a == name) {
        return Some(c);
    }
    if let Some(&c) = COMMANDS.iter().find(|c| **c == name) {
        return Some(c);
    }
    let matches: Vec<&&str> = COMMANDS.iter().filter(|c| c.starts_with(name)).collect();
    if matches.len() == 1 { Some(matches[0]) } else { None }
}

/// amux-only short commands that require an exact match (not prefix-matched).
const EXACT_COMMANDS: &[&str] = &["status", "exec", "await", "events", "panes"];

/// Resolve any built-in server command (tmux commands/aliases/prefixes + exact amux commands).
pub fn lookup_command(name: &str) -> Option<&'static str> {
    resolve_command(name).or_else(|| EXACT_COMMANDS.iter().copied().find(|&c| c == name))
}

/// Whether `name` is reserved by a built-in CLI or server command (or prefix).
pub fn is_reserved_command(name: &str) -> bool {
    lookup_command(name).is_some() || matches!(name, "migrate" | "version")
}

/// Public commands listed by `list-commands` and prompt Tab completion.
pub const PUBLIC_COMMANDS: &[&str] = &[
    "attach-session",
    "bind-key",
    "break-pane",
    "capture-pane",
    "choose-tree",
    "clear-history",
    "command-prompt",
    "confirm-before",
    "copy-mode",
    "delete-buffer",
    "detach-client",
    "display-message",
    "display-panes",
    "has-session",
    "if-shell",
    "join-pane",
    "kill-pane",
    "kill-server",
    "kill-session",
    "kill-window",
    "last-pane",
    "last-window",
    "list-buffers",
    "list-clients",
    "list-commands",
    "list-keys",
    "list-panes",
    "list-sessions",
    "list-windows",
    "load-buffer",
    "move-window",
    "new-session",
    "new-window",
    "next-layout",
    "next-window",
    "paste-buffer",
    "plugin",
    "previous-layout",
    "previous-window",
    "refresh-client",
    "rename-session",
    "rename-window",
    "resize-pane",
    "resize-window",
    "respawn-pane",
    "respawn-window",
    "rotate-window",
    "run-shell",
    "save-buffer",
    "select-layout",
    "select-pane",
    "select-window",
    "send-keys",
    "send-prefix",
    "set-buffer",
    "set-environment",
    "set-option",
    "set-window-option",
    "show-buffer",
    "show-environment",
    "show-messages",
    "show-options",
    "source-file",
    "split-window",
    "suspend-client",
    "swap-pane",
    "swap-window",
    "switch-client",
    "unbind-key",
    "wait-for",
    "agent-view",
    "agent-next",
    "agent-status",
    "agents",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn env(k: &str) -> Option<String> {
        if k == "FOO" { Some("bar".into()) } else { None }
    }

    #[test]
    fn tokenize_tmux_conf_lines() {
        let src = r##"
# comment
setw -g mode-keys vi
bind -T copy-mode-vi v send -X begin-selection
unbind C-b
set -g prefix C-a
bind-key \\ split-window -h -c "#{pane_current_path}"
bind-key ] split-window -v -c "#{pane_current_path}"
bind r source-file ~/.tmux.conf \; display-message "Config reloaded..."
set-environment -g 'SSH_AUTH_SOCK' ~/.ssh/ssh_auth_sock
run-shell "~/.tmux_color.sh"
set -g status-right "$FOO #[fg=red]x"
bind x { kill-pane; display "gone" }
"##;
        let (lines, errs) = parse_config(src, &env, &mut |_| true);
        assert!(errs.is_empty(), "{errs:?}");
        let cmds: Vec<&Argv> = lines.iter().flat_map(|l| l.cmds.iter()).collect();
        assert_eq!(cmds[0], &vec!["setw", "-g", "mode-keys", "vi"]);
        assert_eq!(cmds[4], &vec!["bind-key", "\\", "split-window", "-h", "-c", "#{pane_current_path}"]);
        assert_eq!(cmds[5][1], "]");
        let home = crate::util::home();
        assert_eq!(
            cmds[6],
            &vec![
                "bind".to_string(),
                "r".into(),
                "source-file".into(),
                format!("{home}/.tmux.conf"),
                ";".into(),
                "display-message".into(),
                "Config reloaded...".into()
            ]
        );
        assert_eq!(cmds[7][3], format!("{home}/.ssh/ssh_auth_sock"));
        assert_eq!(cmds[8][1], "~/.tmux_color.sh");
        assert_eq!(cmds[9][3], "bar #[fg=red]x");
        assert_eq!(cmds[10], &vec!["bind", "x", "kill-pane; display \"gone\""]);
        let inner = split_semicolons(&cmds[6][2..]);
        assert_eq!(inner.len(), 2);
    }

    #[test]
    fn semicolons_separate() {
        let l = parse_cmdlist("new -d ; split-window -h", &env).unwrap();
        assert_eq!(l.len(), 2);
    }

    #[test]
    fn if_blocks() {
        let src = "%if \"1\"\nset a 1\n%else\nset a 2\n%endif\n%if \"0\"\nset b 1\n%elif \"1\"\nset b 2\n%endif\n";
        let (lines, _) = parse_config(src, &env, &mut |c| c == "1");
        let v: Vec<String> = lines.iter().map(|l| l.cmds[0].join(" ")).collect();
        assert_eq!(v, vec!["set a 1", "set b 2"]);
    }

    #[test]
    fn getopt() {
        let a = Args::parse(&["-dh".into(), "-c".into(), "/tmp".into(), "-l50%".into(), "vim".into(), "-x".into()], "dhc:l:").unwrap();
        assert!(a.has('d') && a.has('h'));
        assert_eq!(a.get('c'), Some("/tmp"));
        assert_eq!(a.get('l'), Some("50%"));
        assert_eq!(a.pos, vec!["vim", "-x"]);
        assert!(Args::parse(&["-z".into()], "dh").is_err());
    }

    #[test]
    fn aliases() {
        assert_eq!(resolve_command("splitw"), Some("split-window"));
        assert_eq!(resolve_command("new"), Some("new-session"));
        assert_eq!(resolve_command("kill-ses"), Some("kill-session"));
        assert_eq!(resolve_command("select"), None);
    }
}
