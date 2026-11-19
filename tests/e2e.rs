//! End-to-end tests: real server, keepers and CLI in isolated directories.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Env {
    name: String,
    dir: PathBuf,
    config: String,
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_amux")
}

impl Env {
    fn new(tag: &str, config: Option<&str>) -> Env {
        let name = format!("t{}-{}", tag, std::process::id());
        let dir = std::env::temp_dir().join(format!("amux-e2e-{}", name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = config.map(|c| c.to_string()).unwrap_or_else(|| "/dev/null".into());
        Env { name, dir, config }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(bin());
        c.env("AMUX_RUNTIME_DIR", self.dir.join("run"))
            .env("AMUX_STATE_DIR", self.dir.join("state"))
            .env("AMUX_PLUGIN_DIR", self.dir.join("plugins"))
            .env_remove("AMUX_PANE")
            .env_remove("TMUX")
            .env("SHELL", "/bin/sh")
            .args(["-L", &self.name, "-f", &self.config])
            .args(args);
        c
    }

    /// Run a CLI command; a hang (no reply within 30s) fails the test.
    fn run(&self, args: &[&str]) -> Output {
        let child = self.cmd(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("spawn amux");
        let pid = child.id() as i32;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(o) => o.expect("run amux"),
            Err(_) => {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
                panic!("amux {:?} hung", args);
            }
        }
    }

    fn ok(&self, args: &[&str]) -> String {
        let o = self.run(args);
        assert!(o.status.success(), "amux {:?} failed: {}{}", args, String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    fn try_out(&self, args: &[&str]) -> Option<String> {
        let o = self.run(args);
        o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
    }

    /// Write a plugin file, as a coding agent would.
    fn write_plugin(&self, name: &str, text: &str) {
        let dir = self.dir.join("plugins");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), text).unwrap();
    }

    fn stderr(&self, args: &[&str]) -> (Option<i32>, String, String) {
        let o = self.run(args);
        (o.status.code(), String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned())
    }

    fn display(&self, target: &str, fmt: &str) -> String {
        self.ok(&["display", "-p", "-t", target, fmt]).trim().to_string()
    }

    fn capture(&self, target: &str) -> String {
        self.try_out(&["capture-pane", "-p", "-J", "-S", "-", "-t", target]).unwrap_or_default()
    }

    fn until(&self, secs: f64, mut f: impl FnMut() -> bool) -> bool {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs_f64(secs) {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// PIDs of processes whose command line mentions this test's dirs/name.
    fn pids(&self, needle: &str) -> Vec<i32> {
        let mut v = Vec::new();
        for e in std::fs::read_dir("/proc").unwrap().flatten() {
            let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else { continue };
            if let Ok(c) = std::fs::read(e.path().join("cmdline")) {
                let s = String::from_utf8_lossy(&c).replace('\0', " ");
                if s.contains(needle) && pid != std::process::id() as i32 {
                    v.push(pid);
                }
            }
        }
        v
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.run(&["kill-server"]);
        // Let the server finish stopping plugins before killing leftovers.
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(2) && !self.pids(&format!("__server {}", self.name)).is_empty() {
            std::thread::sleep(Duration::from_millis(20));
        }
        let run = self.dir.join("run").to_string_lossy().into_owned();
        let plugins = self.dir.join("plugins").to_string_lossy().into_owned();
        let leftovers = [run, plugins, format!("__supervisor {}", self.name), format!("__server {}", self.name)];
        for p in leftovers.iter().flat_map(|n| self.pids(n)) {
            unsafe {
                libc::kill(p, libc::SIGKILL);
            }
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A fake coding agent named `claude`: works, asks for approval, finishes.
fn fake_claude(dir: &Path) -> PathBuf {
    let script = dir.join("claude");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf '\\033]0;Fixing tests\\007'\nfor i in 1 2 3 4 5 6 7 8; do printf '\\rReading files (%ss - esc to interrupt)' $i; sleep 0.25; done\nprintf '\\r\\033[2K\\nDo you want to proceed?\\n> 1. Yes\\n  2. No\\n'\nread ans\nfor i in 1 2 3 4; do printf '\\rWriting (%ss - esc to interrupt)' $i; sleep 0.25; done\nprintf '\\r\\033[2K\\nAll 42 tests pass.\\n'\nsleep 100\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    script
}

fn last_tick(s: &str) -> u64 {
    s.lines().filter_map(|l| l.trim().strip_prefix("tick ")).filter_map(|n| n.parse().ok()).max().unwrap_or(0)
}

#[test]
fn server_crash_is_invisible_to_panes() {
    let e = Env::new("crash", None);
    e.ok(&["new", "-d", "-s", "t", "i=0; while true; do i=$((i+1)); echo tick $i; sleep 0.05; done"]);
    e.ok(&["split-window", "-d", "-t", "t", "sleep 1000"]);
    assert!(e.until(5.0, || last_tick(&e.capture("t:0.0")) >= 3), "counter never started");
    let pid: i32 = e.display("t", "#{pid}").parse().unwrap();
    let before = last_tick(&e.capture("t:0.0"));
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    // The supervisor restarts the server; it reattaches to the live keepers.
    assert!(
        e.until(10.0, || e.try_out(&["display", "-p", "-t", "t", "#{pid}"]).map(|p| p.trim().parse::<i32>().ok() != Some(pid)).unwrap_or(false)),
        "server did not come back"
    );
    assert_eq!(e.display("t", "#{server_restarts}"), "1");
    assert_eq!(e.ok(&["list-panes", "-t", "t"]).lines().count(), 2, "panes lost");
    assert!(e.until(5.0, || last_tick(&e.capture("t:0.0")) > before + 5), "pane program stopped");
    // History from before the crash is still there.
    assert!(e.capture("t:0.0").contains("tick 1\n"));
}

#[test]
fn killed_keeper_is_resurrected_in_place() {
    let e = Env::new("keeper", None);
    let cwd = e.dir.to_string_lossy().into_owned();
    e.ok(&["new", "-d", "-s", "k", "-c", &cwd]);
    e.ok(&["split-window", "-h", "-d", "-t", "k"]);
    let target = "k:0.0";
    e.ok(&["send-keys", "-t", target, "echo before-loss", "Enter"]);
    assert!(e.until(5.0, || e.capture(target).contains("before-loss\n")));
    let kpid: i32 = e.display(target, "#{pane_keeper_pid}").parse().unwrap();
    assert!(kpid > 0);
    unsafe {
        libc::kill(kpid, libc::SIGKILL);
    }
    assert!(e.until(5.0, || e.capture(target).contains("respawned")), "no respawn: {}", e.capture(target));
    assert_eq!(e.ok(&["list-panes", "-t", "k"]).lines().count(), 2, "layout changed");
    assert_eq!(e.display(target, "#{pane_current_path}"), cwd);
    e.ok(&["send-keys", "-t", target, "echo after-respawn", "Enter"]);
    assert!(e.until(5.0, || e.capture(target).contains("after-respawn\n")));
}

#[test]
fn reboot_restores_layout_and_scrollback() {
    let e = Env::new("reboot", None);
    let cwd = e.dir.to_string_lossy().into_owned();
    e.ok(&["new", "-d", "-s", "r", "-c", &cwd]);
    e.ok(&["split-window", "-v", "-d", "-t", "r"]);
    e.ok(&["new-window", "-d", "-t", "r", "-n", "second"]);
    e.ok(&["send-keys", "-t", "r:0.0", "echo important-history-line", "Enter"]);
    assert!(e.until(5.0, || e.capture("r:0.0").contains("important-history-line\n")));
    // An agent started from the shell in window 1 is resumed (typed, not run).
    let script = fake_claude(&e.dir);
    e.ok(&["send-keys", "-t", "r:1", script.to_str().unwrap(), "Enter"]);
    assert!(e.until(6.0, || e.display("r:1", "#{agent}") == "claude"), "agent not detected");
    e.ok(&["snapshot-panes"]);
    let snaps = e.dir.join("state").join(&e.name).join("panes");
    assert!(
        e.until(5.0, || {
            std::fs::read_dir(&snaps).map(|d| d.flatten().filter(|f| f.path().extension().is_some_and(|x| x == "snap")).count() >= 3).unwrap_or(false)
        }),
        "no snapshots"
    );
    // Simulate a reboot: every process dies and the runtime dir (tmpfs) is gone.
    let run = e.dir.join("run").to_string_lossy().into_owned();
    let mut victims = e.pids(&format!("__supervisor {}", e.name));
    victims.extend(e.pids(&format!("__server {}", e.name)));
    victims.extend(e.pids(&run));
    for p in victims {
        unsafe {
            libc::kill(p, libc::SIGKILL);
        }
    }
    std::thread::sleep(Duration::from_millis(300));
    let _ = std::fs::remove_dir_all(&run);
    // First command after "boot" brings everything back.
    let ls = e.ok(&["ls"]);
    assert!(ls.starts_with("r: 2 windows"), "{ls}");
    assert_eq!(e.ok(&["list-panes", "-t", "r:0"]).lines().count(), 2);
    assert!(e.until(5.0, || e.capture("r:0.0").contains("important-history-line")), "scrollback lost: {}", e.capture("r:0.0"));
    assert!(e.capture("r:0.0").contains("restored"));
    assert_eq!(e.display("r:0.0", "#{pane_current_path}"), cwd);
    assert_eq!(e.display("r:1", "#{window_name}"), "second");
    assert!(e.until(5.0, || e.capture("r:1").trim_end().ends_with("claude --continue")), "resume not typed: {}", e.capture("r:1"));
}

#[test]
fn panics_are_contained() {
    let e = Env::new("panic", None);
    e.ok(&["new", "-d", "-s", "p", "i=0; while true; do i=$((i+1)); echo tick $i; sleep 0.05; done"]);
    assert!(e.until(5.0, || last_tick(&e.capture("p")) >= 3));
    // A bug in a pane's terminal emulator: the keeper rebuilds it from a
    // snapshot and the program never notices.
    e.ok(&["panic-test", "-k", "-t", "p"]);
    assert!(e.until(5.0, || e.display("p", "#{pane_recoveries}") == "1"), "keeper did not recover");
    let before = last_tick(&e.capture("p"));
    assert!(e.until(5.0, || last_tick(&e.capture("p")) > before + 5), "pane program stopped");
    assert!(e.capture("p").contains("tick 1\n"), "history lost");
    // A bug in the server: it is restarted and reattaches to the panes.
    let pid = e.display("p", "#{pid}");
    let _ = e.run(&["panic-test"]);
    assert!(e.until(10.0, || e.try_out(&["display", "-p", "-t", "p", "#{pid}"]).is_some_and(|p| p.trim() != pid)), "server did not restart");
    let before = last_tick(&e.capture("p"));
    assert!(e.until(5.0, || last_tick(&e.capture("p")) > before + 5));
}

#[test]
fn orphan_keepers_are_adopted() {
    let e = Env::new("orphan", None);
    e.ok(&["new", "-d", "-s", "o"]);
    e.ok(&["send-keys", "-t", "o", "echo orphan-marker", "Enter"]);
    assert!(e.until(5.0, || e.capture("o").contains("orphan-marker\n")));
    // Lose the server and its state file, but not the pane processes.
    let mut victims = e.pids(&format!("__supervisor {}", e.name));
    victims.extend(e.pids(&format!("__server {}", e.name)));
    for p in victims {
        unsafe {
            libc::kill(p, libc::SIGKILL);
        }
    }
    std::thread::sleep(Duration::from_millis(200));
    std::fs::remove_file(e.dir.join("state").join(&e.name).join("state.json")).unwrap();
    e.ok(&["start-server"]);
    assert!(e.until(5.0, || e.try_out(&["ls"]).is_some_and(|l| l.starts_with("recovered:"))), "{:?}", e.try_out(&["ls"]));
    assert!(e.until(5.0, || e.capture("recovered").contains("orphan-marker")));
}

#[test]
fn users_tmux_conf_loads_cleanly() {
    let conf = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/user.tmux.conf");
    let e = Env::new("conf", Some(conf.to_str().unwrap()));
    e.ok(&["new", "-d", "-s", "c"]);
    assert_eq!(e.ok(&["show", "-gv", "prefix"]).trim(), "C-a");
    assert_eq!(e.ok(&["show", "-gv", "mode-keys"]).trim(), "vi");
    assert_eq!(e.ok(&["show", "-gv", "history-limit"]).trim(), "10000");
    let keys = e.ok(&["list-keys"]);
    assert!(keys.contains("split-window -h -c \"#{pane_current_path}\""), "{keys}");
    assert!(keys.contains("-T copy-mode-vi") && keys.contains("send-keys -X begin-selection"));
    assert!(keys.contains("-T root") && keys.contains("clear-history"));
    // run-shell executes `tmux set ...` through the shim against this server.
    assert!(e.until(5.0, || e.ok(&["show", "-gv", "status-style"]).contains("colour33")), "run-shell/tmux shim did not apply");
    let msgs = e.ok(&["show-messages"]);
    assert!(!msgs.contains("unknown") && !msgs.contains("invalid"), "config errors: {msgs}");
    // A split with the user's binding opens in the current pane's directory.
    e.ok(&["send-keys", "-t", "c", "cd /tmp", "Enter"]);
    assert!(e.until(5.0, || e.display("c", "#{pane_current_path}") == "/tmp"));
    e.ok(&["split-window", "-h", "-t", "c", "-c", "#{pane_current_path}"]);
    assert_eq!(e.display("c", "#{pane_current_path}"), "/tmp");
}

#[test]
fn agents_are_detected_and_tracked() {
    let e = Env::new("agents", None);
    let script = fake_claude(&e.dir);
    e.ok(&["new", "-d", "-s", "a", script.to_str().unwrap()]);
    let state = |e: &Env| -> String {
        let j = e.ok(&["agents", "--json"]);
        let v: serde_json::Value = serde_json::from_str(&j).unwrap_or(serde_json::json!([]));
        v.get(0).and_then(|a| a.get("state")).and_then(|s| s.as_str()).unwrap_or("").to_string()
    };
    assert!(e.until(6.0, || state(&e) == "working"), "never working: {}", e.ok(&["agents"]));
    let j = e.ok(&["agents", "--json"]);
    assert!(j.contains("\"agent\": \"claude\""), "{j}");
    assert!(e.until(6.0, || state(&e) == "waiting"), "never waiting: {}", e.ok(&["agents"]));
    assert!(e.ok(&["agents"]).contains("Do you want to proceed?"));
    e.ok(&["send-keys", "-t", "a", "1", "Enter"]);
    assert!(e.until(3.0, || state(&e) == "working"));
    assert!(e.until(8.0, || state(&e) == "done"), "never done: {}", e.ok(&["agents"]));
    let out = e.ok(&["agents"]);
    assert!(out.contains("[Fixing tests] All 42 tests pass."), "{out}");
    // Any pane can report status explicitly (agent hooks). A mark set from
    // outside the pane survives its process reports (the first one, a new
    // working directory, other programs) until it is cleared.
    let pid = e.ok(&["new-window", "-d", "-P", "-F", "#{pane_id}", "-t", "a", "sh -c 'sleep 0.3; cd /tmp; sleep 0.3; exec sh'"]).trim().to_string();
    e.ok(&["status", "set", "waiting", "needs review of PR 12", "-t", &pid]);
    assert!(e.until(5.0, || e.display(&pid, "#{pane_current_path}") == "/tmp"), "no process report");
    std::thread::sleep(Duration::from_millis(1100));
    let out = e.ok(&["agents"]);
    assert!(out.contains("needs review of PR 12"), "{out}");
    e.ok(&["status", "clear", "-t", &pid]);
    // A script that marks itself owns the mark: kept while it runs (also
    // after its process report arrives), gone when it ends.
    let job = e.ok(&["new-window", "-d", "-P", "-F", "#{pane_id}", "-t", "a"]).trim().to_string();
    let mark = format!("sh -c 'AMUX_RUNTIME_DIR={} {} status set working building-now; sleep 2'", e.dir.join("run").display(), bin());
    e.ok(&["send-keys", "-t", &job, &mark, "Enter"]);
    assert!(e.until(5.0, || e.ok(&["agents"]).contains("building-now")), "script mark missing: {}\n{}", e.ok(&["agents"]), e.capture(&job));
    std::thread::sleep(Duration::from_millis(700));
    assert!(e.ok(&["agents"]).contains("building-now"), "script mark lost while it runs");
    assert!(e.until(6.0, || !e.ok(&["agents"]).contains("building-now")), "mark outlived the script");
}

#[test]
fn exec_and_await_api_for_agents() {
    let e = Env::new("api", None);
    e.ok(&["new", "-d", "-s", "w"]);
    let o = e.run(&["exec", "--", "sh", "-c", "echo hello-from-run; exit 3"]);
    assert_eq!(o.status.code(), Some(3), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stdout).contains("hello-from-run"));
    let pane = e.ok(&["new-window", "-d", "-P", "-F", "#{pane_id}", "-t", "w", "sleep 0.5; echo READY-42; sleep 100"]).trim().to_string();
    let o = e.run(&["await", "-t", &pane, "--pattern", "READY-[0-9]+", "--timeout", "5"]);
    assert!(o.status.success());
    assert!(String::from_utf8_lossy(&o.stdout).contains("READY-42"));
    let o = e.run(&["await", "-t", &pane, "--pattern", "never-appears", "--timeout", "0.5"]);
    assert_eq!(o.status.code(), Some(124));
    let o = e.run(&["await", "-t", &pane, "--idle", "0.3", "--timeout", "5"]);
    assert!(o.status.success());
}

#[test]
fn layouts_and_pane_commands() {
    let e = Env::new("layout", None);
    e.ok(&["new", "-d", "-s", "l", "-x", "120", "-y", "41"]);
    e.ok(&["split-window", "-h", "-t", "l"]);
    e.ok(&["split-window", "-v", "-t", "l"]);
    assert_eq!(e.display("l", "#{window_panes}"), "3");
    e.ok(&["select-layout", "-t", "l", "even-horizontal"]);
    let widths: Vec<u32> = e.ok(&["list-panes", "-t", "l", "-F", "#{pane_width}"]).lines().map(|l| l.parse().unwrap()).collect();
    assert_eq!(widths, vec![39, 39, 40]);
    assert!(widths.iter().max().unwrap() - widths.iter().min().unwrap() <= 1);
    e.ok(&["select-layout", "-t", "l", "tiled"]);
    e.ok(&["resize-pane", "-Z", "-t", "l"]);
    assert_eq!(e.display("l", "#{window_zoomed_flag}"), "1");
    e.ok(&["resize-pane", "-Z", "-t", "l"]);
    e.ok(&["kill-pane", "-t", "l:0.2"]);
    assert_eq!(e.display("l", "#{window_panes}"), "2");
    e.ok(&["break-pane", "-d", "-t", "l:0.1"]);
    assert_eq!(e.ok(&["list-windows", "-t", "l"]).lines().count(), 2);
    e.ok(&["rename-window", "-t", "l:1", "logs"]);
    assert_eq!(e.display("l:1", "#{window_name}"), "logs");
    e.ok(&["kill-window", "-t", "l:logs"]);
    assert_eq!(e.ok(&["list-windows", "-t", "l"]).lines().count(), 1);
    e.ok(&["set-buffer", "-b", "b1", "hello buffer"]);
    assert_eq!(e.ok(&["show-buffer", "-b", "b1"]).trim(), "hello buffer");
    assert!(e.run(&["has-session", "-t", "nope"]).status.code() == Some(1));
}

#[test]
fn cursor_and_mode_changes_alone_reach_the_server() {
    // Nothing on screen changes: only the cursor moves, then a mode is set
    // (as when a shell moves the cursor, or a program asks for the mouse).
    let e = Env::new("cursor", None);
    let prog = r#"sh -c 'stty -echo; printf abc; read x; printf "\b\b"; read y; printf "\033[?1003h"; exec cat'"#;
    e.ok(&["new-session", "-d", "-s", "c", "-x", "80", "-y", "24", prog]);
    assert!(e.until(5.0, || e.display("c:", "#{cursor_x}") == "3"));
    e.ok(&["send-keys", "-t", "c:", "Enter"]);
    assert!(e.until(5.0, || e.display("c:", "#{cursor_x}") == "1"), "cursor_x {}", e.display("c:", "#{cursor_x}"));
    assert_eq!(e.display("c:", "#{mouse_any_flag}"), "0");
    e.ok(&["send-keys", "-t", "c:", "Enter"]);
    assert!(e.until(5.0, || e.display("c:", "#{mouse_any_flag}") == "1"));
    assert_eq!(e.display("c:", "#{cursor_flag} #{keypad_cursor_flag} #{wrap_flag}"), "1 0 1");
}

#[test]
fn plugins_load_hot_reload_and_unwind() {
    let e = Env::new("plugins", None);
    e.ok(&["new", "-d", "-s", "p"]);
    // An agent writes a plugin file: it comes alive without a restart, and its
    // command can be used right away (amux waits for the load in progress).
    e.write_plugin(
        "greet.py",
        r#"
import amux_plugin as amux

@amux.command("greet", help="say hello")
def greet(ctx, *args):
    session = amux.run("display-message -p '#{session_name}'").strip()
    return "v1 hello %s in %s" % (" ".join(args), session)

amux.status("S-ONE")
amux.key("F7", "greet from-key", table="root")
amux.main()
"#,
    );
    assert_eq!(e.ok(&["greet", "you"]).trim(), "v1 hello you in p");
    assert!(e.display("p", "#{amux_plugins}").contains("S-ONE"));
    assert!(e.ok(&["list-keys", "-T", "root"]).contains("greet from-key"));
    // Saving a new version hot-reloads it, and v1's effects are undone.
    e.write_plugin("greet.py", "import amux_plugin as amux\n@amux.command('greet')\ndef greet(ctx, *a):\n    return 'v2'\namux.main()\n");
    assert!(e.until(5.0, || e.try_out(&["greet"]).is_some_and(|o| o.contains("v2"))), "no hot reload");
    assert_eq!(e.display("p", "#{amux_plugins}"), "", "status segment not unwound");
    assert!(!e.ok(&["list-keys", "-T", "root"]).contains("greet"), "key binding not unwound");
    // A crash fails the request, not amux; the plugin is restarted.
    e.write_plugin(
        "greet.py",
        "import os\nimport amux_plugin as amux\n@amux.command('greet')\ndef greet(ctx, *a):\n    if a: os._exit(3)\n    return 'v3'\namux.main()\n",
    );
    assert!(e.until(5.0, || e.try_out(&["greet"]).is_some_and(|o| o.contains("v3"))));
    let (code, _, err) = e.stderr(&["greet", "die"]);
    assert_eq!(code, Some(1));
    assert!(err.contains("exited (status 3)"), "{err}");
    assert!(e.until(6.0, || e.try_out(&["greet"]).is_some_and(|o| o.contains("v3"))), "not restarted");
    assert!(e.ok(&["plugin", "list"]).contains("1 restart"));
    // A broken version is reported with its traceback and stays off.
    e.write_plugin("greet.py", "import amux_plugin as amux\ndef broken(:\n");
    let (code, out, _) = e.stderr(&["plugin", "reload", "greet"]);
    assert_eq!(code, Some(1));
    assert!(out.contains("greet: failed") && out.contains("SyntaxError"), "{out}");
    let (_, _, err) = e.stderr(&["greet"]);
    assert!(err.contains("plugin greet is failed"), "{err}");
    // Deleting the file removes the plugin.
    std::fs::remove_file(e.dir.join("plugins/greet.py")).unwrap();
    assert!(e.until(5.0, || !e.ok(&["plugin", "list"]).contains("greet")));
    assert_eq!(e.stderr(&["greet"]).0, Some(1));
}

#[test]
fn plugins_scaffold_events_effects_and_examples() {
    let e = Env::new("pluginkit", None);
    e.ok(&["new", "-d", "-s", "k"]);
    // `plugin new` writes a working plugin and waits until it is up.
    let out = e.ok(&["plugin", "new", "demo"]);
    assert!(out.contains("demo: running"), "{out}");
    assert!(e.ok(&["demo", "there"]).contains("hello, there!"));
    assert!(e.ok(&["plugin", "new", "shy", "--lang", "sh"]).contains("shy: running"));
    assert!(e.ok(&["shy"]).contains("shell plugin"));
    // Events reach plugins; annotations and agent statuses are their effects.
    e.write_plugin(
        "watch.py",
        r#"
import amux_plugin as amux

@amux.on("agent-state")
def changed(ev):
    amux.annotate(ev.pane, "seen-" + ev.state)

@amux.command("mark")
def mark(ctx, *args):
    amux.run("status", "set", "working", "marked by plugin")
    return "ok"

amux.main()
"#,
    );
    assert!(e.ok(&["plugin", "reload", "watch"]).contains("watch: running"));
    let pane = e.display("k", "#{pane_id}");
    e.ok(&["status", "set", "waiting", "real event", "-t", &pane]);
    assert!(e.until(5.0, || e.ok(&["agents", "--json"]).contains("seen-waiting")), "{}", e.ok(&["agents", "--json"]));
    // `demo` (the template) and `watch` both subscribe to agent-state.
    assert!(e.ok(&["plugin", "emit", "agent-state", "-t", &pane, "state=done"]).contains("sent to 2 plugin"));
    assert!(e.until(5.0, || e.ok(&["agents", "--json"]).contains("seen-done")));
    e.ok(&["status", "clear", "-t", &pane]);
    assert_eq!(e.ok(&["mark"]).trim(), "ok");
    assert!(e.ok(&["agents"]).contains("marked by plugin"));
    e.ok(&["plugin", "remove", "watch"]);
    assert!(
        e.until(5.0, || !e.ok(&["agents"]).contains("marked by plugin") && !e.ok(&["agents", "--json"]).contains("seen-")),
        "effects not unwound: {}",
        e.ok(&["agents", "--json"])
    );
    // Example plugins install from a path; worktree drives git and windows.
    let ex = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/plugins/worktree.py");
    assert!(e.ok(&["plugin", "install", ex.to_str().unwrap()]).contains("worktree: running"));
    let repo = e.dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("a.txt"), "hi\n").unwrap();
    for args in [
        &["init", "-q"][..],
        &["-c", "user.email=t@t", "-c", "user.name=t", "add", "."],
        &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"],
    ] {
        assert!(Command::new("git").arg("-C").arg(&repo).args(args).status().unwrap().success());
    }
    e.ok(&["new-window", "-t", "k", "-c", repo.to_str().unwrap()]);
    assert!(e.ok(&["worktree", "feat-x"]).contains("branch feat-x"));
    assert!(e.dir.join("repo-feat-x/a.txt").exists());
    assert!(e.ok(&["list-windows", "-t", "k", "-F", "#{window_name}"]).contains("feat-x"));
}

#[test]
fn plugins_are_contained_and_survive_server_crashes() {
    let e = Env::new("pluginsafe", None);
    e.ok(&["new", "-d", "-s", "s"]);
    // A raw-protocol plugin that ignores SIGTERM, prints junk to stdout and
    // leaves a helper process running.
    let stubborn = |v: &str| {
        format!(
            "#!/bin/sh\ntrap '' TERM\nsleep 1000 &\necho $! > \"$AMUX_PLUGIN_DATA/helper\"\necho 'not json'\necho '{{\"method\":\"register\",\"params\":{{\"commands\":[\"stub{v}\",\"send-keys\"]}}}}'\nwhile :; do sleep 0.1; done\n"
        )
    };
    let helper = || -> i32 {
        let state = std::fs::read_dir(e.dir.join("state")).into_iter().flatten().flatten();
        let file = state.map(|d| d.path().join("plugin-data/stub/helper")).find(|p| p.exists());
        file.and_then(|p| std::fs::read_to_string(p).ok()).and_then(|s| s.trim().parse().ok()).unwrap_or(0)
    };
    let alive =
        |pid: i32| pid > 0 && unsafe { libc::kill(pid, 0) } == 0 && !std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default().contains(") Z");
    e.write_plugin("stub.sh", &stubborn("1"));
    // A rejected registration is reported (exit 1) but the rest works.
    let (code, out, _) = e.stderr(&["plugin", "reload", "stub"]);
    assert_eq!(code, Some(1));
    assert!(out.contains("stub: running") && out.contains("rejected send-keys"), "{out}");
    let logs = e.ok(&["plugin", "logs", "stub"]);
    assert!(logs.contains("ignored: not json"), "{logs}");
    let first_helper = helper();
    assert!(alive(first_helper));
    // Reloading kills it (and its helper) even though it ignores SIGTERM.
    e.write_plugin("stub.sh", &stubborn("2"));
    let t0 = Instant::now();
    assert!(e.until(5.0, || e.ok(&["plugin", "show", "stub"]).contains("command   stub2")));
    assert!(t0.elapsed() < Duration::from_secs(4));
    assert!(e.until(3.0, || !alive(first_helper)), "helper survived a reload");
    let pid_of = |e: &Env| -> i32 {
        let show = e.ok(&["plugin", "show", "stub"]);
        show.split("pid ").nth(1).and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next()).and_then(|n| n.parse().ok()).unwrap_or(0)
    };
    let old = pid_of(&e);
    assert!(old > 0);
    let old_helper = helper();
    assert!(alive(old_helper) && old_helper != first_helper);
    // The server crashes: its plugins die with it and come back with the new one.
    let server: i32 = e.display("s", "#{pid}").parse().unwrap();
    unsafe {
        libc::kill(server, libc::SIGKILL);
    }
    assert!(e.until(10.0, || e.try_out(&["plugin", "show", "stub"]).is_some_and(|o| o.contains("state     running"))), "plugin not restarted");
    assert_ne!(pid_of(&e), old);
    assert!(e.until(3.0, || !alive(old)), "old plugin process survived the server");
    // ...and what it left behind is cleaned up by the new server.
    assert!(e.until(3.0, || !alive(old_helper)), "old plugin's helper survived the server");
    // A clean shutdown stops plugins and their helpers within moments, and
    // a server started right away (while the old one still stops a plugin
    // that ignores `shutdown`) works.
    let new_helper = helper();
    assert!(alive(new_helper));
    let t0 = Instant::now();
    e.ok(&["kill-server"]);
    e.ok(&["new", "-d", "-s", "again"]);
    assert_eq!(e.ok(&["ls", "-F", "#{session_name}"]).trim(), "again");
    assert!(e.until(3.0, || !alive(new_helper)), "helper survived kill-server");
    assert!(t0.elapsed() < Duration::from_secs(3));
}

#[test]
fn plugins_teach_amux_new_agents() {
    let e = Env::new("pluginagents", None);
    // A coding agent amux has never heard of, waiting for the user.
    let agent = e.dir.join("fakeagent");
    std::fs::write(&agent, "#!/bin/sh\nprintf 'Plan ready.\\nawaiting go-ahead\\n'\nwhile :; do sleep 1; done\n").unwrap();
    std::fs::set_permissions(&agent, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    e.ok(&["new", "-d", "-s", "a", agent.to_str().unwrap()]);
    assert!(e.until(5.0, || e.display("a", "#{pane_current_command}") == "sh"), "no process info");
    assert!(!e.ok(&["agents", "--json"]).contains("fakeagent"));
    // A plugin defines it: the running pane is recognised and its prompt read.
    e.write_plugin(
        "agentdef.py",
        r#"
import amux_plugin as amux
amux.agent("fakeagent", cmd=r"fakeagent", waiting=[r"awaiting go-ahead"], resume="fakeagent --resume")
amux.main()
"#,
    );
    let out = e.ok(&["plugin", "reload", "agentdef"]);
    assert!(out.contains("agents: fakeagent"), "{out}");
    let waiting = || {
        let j = e.ok(&["agents", "--json"]);
        j.contains("\"agent\": \"fakeagent\"") && j.contains("\"state\": \"waiting\"") && j.contains("awaiting go-ahead")
    };
    assert!(e.until(5.0, waiting), "{}", e.ok(&["agents", "--json"]));
    let kinds = e.ok(&["agents", "--kinds"]);
    assert!(kinds.lines().any(|l| l.starts_with("fakeagent") && l.contains("plugin agentdef") && l.contains("fakeagent --resume")), "{kinds}");
    assert!(kinds.lines().any(|l| l.starts_with("claude") && l.contains("built-in")), "{kinds}");
    // Bad definitions are rejected with a reason.
    e.write_plugin("bad.py", "import amux_plugin as amux\namux.agent('shellish', cmd='sh')\namux.main()\n");
    let (code, out, _) = e.stderr(&["plugin", "reload", "bad"]);
    assert_eq!(code, Some(1));
    assert!(out.contains("rejected agent shellish") && out.contains("plain shell"), "{out}");
    // Unloading the plugin undoes the definition.
    let state = e.dir.join("state").join(&e.name).join("state.json");
    assert!(e.until(5.0, || std::fs::read_to_string(&state).unwrap_or_default().contains("\"agent_resume\": \"fakeagent --resume\"")), "resume not saved");
    e.ok(&["plugin", "remove", "agentdef"]);
    assert!(e.until(5.0, || !e.ok(&["agents", "--json"]).contains("fakeagent")), "definition not unwound");
    assert!(!e.ok(&["agents", "--kinds"]).contains("fakeagent"));
    // After a reboot the agent is resumed from a shell (plugins load only
    // after panes are restored, so the saved resume command is used).
    let run = e.dir.join("run").to_string_lossy().into_owned();
    let mut victims = e.pids(&format!("__supervisor {}", e.name));
    victims.extend(e.pids(&format!("__server {}", e.name)));
    victims.extend(e.pids(&run));
    for p in victims {
        unsafe {
            libc::kill(p, libc::SIGKILL);
        }
    }
    std::thread::sleep(Duration::from_millis(300));
    let _ = std::fs::remove_dir_all(&run);
    assert!(e.ok(&["ls"]).starts_with("a: 1 windows"));
    assert!(e.until(5.0, || e.capture("a").trim_end().ends_with("fakeagent --resume")), "resume not typed: {}", e.capture("a"));
    let cap = e.capture("a");
    let after_restore = cap.rsplit("restored").next().unwrap_or("");
    assert!(!after_restore.contains("awaiting go-ahead"), "the agent was started again instead of a shell: {cap}");
}

fn tmux_ok() -> bool {
    Command::new("tmux").arg("-V").output().map(|o| o.status.success()).unwrap_or(false)
}

/// A real terminal for UI tests: amux attached inside a private tmux server.
struct Outer {
    sock: String,
}

impl Outer {
    fn start(e: &Env, session: &str) -> Outer {
        let sock = e.dir.join("ui.sock").to_string_lossy().into_owned();
        let attach = format!(
            "env AMUX_RUNTIME_DIR={} AMUX_STATE_DIR={} AMUX_PLUGIN_DIR={} SHELL=/bin/sh {} -L {} attach -t {}",
            e.dir.join("run").display(),
            e.dir.join("state").display(),
            e.dir.join("plugins").display(),
            bin(),
            e.name,
            session
        );
        let ok =
            Command::new("tmux").args(["-S", &sock, "-f", "/dev/null", "new", "-d", "-s", "ui", "-x", "120", "-y", "40", &attach]).env_remove("TMUX").status();
        assert!(ok.is_ok_and(|s| s.success()), "cannot start tmux");
        let o = Outer { sock };
        assert!(e.until(5.0, || o.screen().contains(&format!("[{}]", session))), "not attached: {}", o.screen());
        o
    }

    fn keys(&self, keys: &[&str]) {
        let _ = Command::new("tmux").args(["-S", &self.sock, "send-keys", "-t", "ui"]).args(keys).env_remove("TMUX").status();
    }

    fn screen(&self) -> String {
        let o = Command::new("tmux").args(["-S", &self.sock, "capture-pane", "-p", "-t", "ui"]).env_remove("TMUX").output();
        o.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default()
    }
}

impl Drop for Outer {
    fn drop(&mut self) {
        let _ = Command::new("tmux").args(["-S", &self.sock, "kill-server"]).env_remove("TMUX").output();
        let _ = std::fs::remove_file(&self.sock);
    }
}

#[test]
fn plugin_view_shows_state_logs_and_trace() {
    let e = Env::new("pluginview", None);
    e.ok(&["new", "-d", "-s", "s"]);
    e.write_plugin(
        "noisy.py",
        "import amux_plugin as amux\n@amux.command('noisy')\ndef noisy(ctx, *a):\n    print('\\x1b[31mnoisy\\x1b[0m handled', *a)\n    return 'ok'\namux.main()\n",
    );
    e.write_plugin("broken.py", "import amux_plugin as amux\ndef broken(:\n");
    let _ = e.run(&["plugin", "reload"]);
    assert_eq!(e.ok(&["noisy", "x1"]).trim(), "ok");
    // The protocol trace: what amux sent and received, and lifecycle events.
    let trace = e.ok(&["plugin", "trace", "noisy"]);
    for want in ["· started (pid", "← {", "\"method\":\"register\"", "→ {", "\"method\":\"command\"", "\"result\":\"ok\""] {
        assert!(trace.contains(want), "{want} missing:\n{trace}");
    }
    // Without a terminal attached, the view cannot open.
    let (code, _, err) = e.stderr(&["plugin", "view"]);
    assert_eq!(code, Some(1));
    assert!(err.contains("attached"), "{err}");
    if !tmux_ok() {
        eprintln!("tmux not found; skipping the UI part");
        return;
    }
    // prefix P opens it on the plugin that needs attention.
    let ui = Outer::start(&e, "s");
    ui.keys(&["C-b", "P"]);
    assert!(e.until(5.0, || ui.screen().contains("PLUGINS")), "{}", ui.screen());
    let s = ui.screen();
    assert!(s.contains("✗ broken") && s.contains("● noisy") && s.contains("SyntaxError"), "{s}");
    // The selected plugin's live log, without escape sequences.
    ui.keys(&["j"]);
    assert!(e.until(5.0, || ui.screen().contains("noisy handled x1")), "{}", ui.screen());
    // t: protocol trace; enter: full screen; q: back; q: close.
    ui.keys(&["t"]);
    assert!(e.until(3.0, || ui.screen().contains("protocol trace")), "{}", ui.screen());
    ui.keys(&["Enter"]);
    assert!(e.until(3.0, || ui.screen().contains("PLUGINS › noisy")), "{}", ui.screen());
    ui.keys(&["q"]);
    assert!(
        e.until(3.0, || {
            let s = ui.screen();
            s.contains("PLUGINS") && !s.contains("PLUGINS ›")
        }),
        "{}",
        ui.screen()
    );
    ui.keys(&["q"]);
    assert!(e.until(3.0, || !ui.screen().contains("PLUGINS")), "{}", ui.screen());
}

/// A private tmux server for migration tests. Never the user's: its socket
/// is in the test directory, every command names it, and TMUX_TMPDIR points
/// there too, so even a bare `tmux` could not reach another server.
struct TmuxSrv {
    sock: PathBuf,
    dir: PathBuf,
}

impl TmuxSrv {
    fn start(e: &Env) -> TmuxSrv {
        let sock = e.dir.join("tmux.sock");
        assert!(sock.starts_with(std::env::temp_dir()));
        TmuxSrv { sock, dir: e.dir.clone() }
    }

    fn cmd(&self, args: &[&str]) -> Output {
        Command::new("tmux")
            .arg("-S")
            .arg(&self.sock)
            .args(args)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env("TMUX_TMPDIR", &self.dir)
            .env("PROMPT_COMMAND", "")
            .env("PS1", "$ ")
            .env("ENV", "/dev/null")
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let o = self.cmd(args);
        assert!(o.status.success(), "tmux {:?}: {}", args, String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    fn path(&self) -> &str {
        self.sock.to_str().unwrap()
    }

    /// What must not change: sessions, windows, layouts, processes,
    /// directories, buffers, session environments and the text of every
    /// pane (except `skip`).
    fn snapshot(&self, skip: &[&str]) -> String {
        let fmt = "#{session_name} #{session_windows} #{window_index} #{window_name} #{window_layout} #{window_active} #{pane_id} #{pane_pid} #{pane_active} #{pane_current_path} #{pane_dead}";
        let panes = self.ok(&["list-panes", "-a", "-F", fmt]);
        let mut s = panes.clone();
        for l in panes.lines() {
            let id = l.split(' ').nth(6).unwrap();
            if !skip.contains(&id) {
                s.push_str(&self.ok(&["capture-pane", "-p", "-S", "-", "-t", id]));
            }
        }
        s.push_str(&self.ok(&["list-buffers"]));
        for name in self.ok(&["list-sessions", "-F", "#{session_name}"]).lines() {
            s.push_str(&self.ok(&["show-environment", "-t", name]));
        }
        s
    }

    fn clients(&self) -> usize {
        self.ok(&["list-clients"]).lines().count()
    }
}

impl Drop for TmuxSrv {
    fn drop(&mut self) {
        let _ = self.cmd(&["kill-server"]);
        let _ = std::fs::remove_file(&self.sock);
    }
}

/// tmux with shells at a prompt (one with history), `cat` (a live program),
/// and a program that asked for bracketed paste.
fn tmux_fixture(e: &Env) -> TmuxSrv {
    let t = TmuxSrv::start(e);
    let (a, b) = (e.dir.join("a"), e.dir.join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let (a, b) = (a.to_str().unwrap(), b.to_str().unwrap());
    t.ok(&[
        "-f",
        "/dev/null",
        "start-server",
        ";",
        "set-option",
        "-g",
        "default-shell",
        "/bin/sh",
        ";",
        "new-session",
        "-d",
        "-s",
        "work",
        "-x",
        "100",
        "-y",
        "30",
        "-n",
        "edit",
        "-c",
        a,
    ]);
    t.ok(&["split-window", "-h", "-t", "work:edit", "-c", b]);
    t.ok(&["new-window", "-t", "work", "-n", "live", "-c", b, "exec cat"]);
    t.ok(&["new-window", "-t", "work", "-n", "paste", "-c", a, "printf '\\033[?2004h\\033[?25l'; exec cat -v"]);
    // Attaching normally copies update-environment variables into the
    // session: a bridge must leave this one alone.
    t.ok(&["set-environment", "-t", "work", "SSH_AUTH_SOCK", "/fixture/agent.sock"]);
    t.ok(&["send-keys", "-t", "work:edit.0", "echo history-line-1; echo history-line-2", "Enter"]);
    // Leading newline matches echo's output, not the echoed command line; also
    // wait until every pane has finished spawning (e.g. systemd cgroup setup).
    assert!(e.until(5.0, || {
        t.ok(&["capture-pane", "-p", "-t", "work:edit.0"]).contains("\nhistory-line-2\n")
            && t.ok(&["list-panes", "-s", "-t", "work", "-F", "#{pane_current_command}"]).lines().collect::<Vec<_>>() == ["sh", "sh", "cat", "cat"]
    }));
    // The current window has no live pane: any bridge that selected its own
    // pane's window in tmux would show up as a change.
    t.ok(&["select-window", "-t", "work:edit"]);
    t
}

#[test]
fn migrate_moves_tmux_sessions_without_changing_tmux() {
    if !tmux_ok() {
        eprintln!("tmux not found; skipping");
        return;
    }
    let e = Env::new("migrate", None);
    let t = tmux_fixture(&e);
    let before = t.snapshot(&[]);
    // For later: everything but the live panes, which will be typed into.
    let untyped = t.snapshot(&["%2", "%3"]);
    // The plan changes nothing anywhere; no amux server even starts.
    let plan = e.ok(&["migrate", "-S", t.path()]);
    for want in ["%0    fresh sh", "%2    live  cat", "%3    live  cat", "Nothing has changed"] {
        assert!(plan.contains(want), "{want} missing:\n{plan}");
    }
    assert!(!e.run(&["ls"]).status.success(), "the plan started amux");
    // Run from inside tmux pane %2, migrate must not show that pane live:
    // amux would show itself inside itself.
    let tmux_pid = t.ok(&["display", "-p", "#{pid}"]).trim().to_string();
    let inside = format!("{},{},0", t.path(), tmux_pid);
    let o = e.cmd(&["migrate"]).env("TMUX", &inside).env("TMUX_PANE", "%2").output().unwrap();
    let plan_inside = String::from_utf8_lossy(&o.stdout);
    assert!(plan_inside.contains("%2    fresh cat") && plan_inside.contains("%2 runs amux"), "{plan_inside}");
    assert!(plan_inside.contains("%3    live  cat"), "{plan_inside}");
    assert_eq!(t.snapshot(&[]), before);
    let out = e.ok(&["migrate", "-S", t.path(), "--apply"]);
    assert!(out.contains("work → amux session work: 3 window(s), 2 live pane(s), 2 fresh"), "{out}");
    // Same windows, pane sizes and directories as in tmux.
    assert_eq!(e.ok(&["list-windows", "-t", "work", "-F", "#{window_index} #{window_name}"]), "0 edit\n1 live\n2 paste\n");
    let geometry = "#{window_index} #{pane_width}x#{pane_height} #{pane_current_path}";
    let theirs = t.ok(&["list-panes", "-s", "-t", "work", "-F", geometry]);
    assert!(
        e.until(5.0, || e.ok(&["list-panes", "-s", "-t", "work", "-F", geometry]) == theirs),
        "{}\nvs\n{theirs}",
        e.ok(&["list-panes", "-s", "-t", "work", "-F", geometry])
    );
    // Fresh panes start below their tmux history.
    assert!(e.until(5.0, || {
        let c = e.capture("work:edit.0");
        c.contains("history-line-2") && c.contains("migrated from tmux work %0")
    }));
    // The live pane is the same process, and what is typed in amux reaches it.
    let cat = t.ok(&["display", "-p", "-t", "work:live", "#{pane_pid}"]);
    e.ok(&["send-keys", "-t", "work:live", "hello bridge", "Enter"]);
    assert!(e.until(5.0, || t.ok(&["capture-pane", "-p", "-t", "work:live"]).matches("hello bridge").count() == 2), "input did not reach tmux");
    assert!(e.until(5.0, || e.capture("work:live").matches("hello bridge").count() == 2), "{}", e.capture("work:live"));
    assert_eq!(t.ok(&["display", "-p", "-t", "work:live", "#{pane_pid}"]), cat);
    // An amux client inside a tmux pane that amux shows live would loop:
    // what it draws and every key typed would go round forever.
    let o = e.cmd(&["attach", "-t", "work"]).env("TMUX", &inside).env("TMUX_PANE", "%2").stdin(Stdio::null()).output().unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(!o.status.success() && err.contains("tmux pane %2") && err.contains("live"), "{err}");
    let o = e.cmd(&["attach", "-t", "work"]).env("TMUX", &inside).env("TMUX_PANE", "%0").stdin(Stdio::null()).output().unwrap();
    assert!(String::from_utf8_lossy(&o.stderr).contains("not a terminal"), "{}", String::from_utf8_lossy(&o.stderr));
    // A paste arrives bracketed (the program asked for it), via a tmux buffer
    // that is deleted again.
    // (The bridge paints the hidden cursor after bracketed paste: once amux
    // shows it, amux has seen both.)
    assert!(e.until(5.0, || e.display("work:paste", "#{cursor_flag}") == "0"));
    e.ok(&["set-buffer", "p-one"]);
    e.ok(&["paste-buffer", "-p", "-t", "work:paste"]);
    assert!(
        e.until(5.0, || t.ok(&["capture-pane", "-p", "-t", "work:paste"]).contains("^[[200~p-one^[[201~")),
        "{}",
        t.ok(&["capture-pane", "-p", "-t", "work:paste"])
    );
    assert_eq!(t.ok(&["list-buffers"]), "");
    // Apart from what was typed into live panes, tmux is as it was; stopping
    // amux leaves no client behind.
    assert_eq!(t.snapshot(&["%2", "%3"]), untyped);
    e.ok(&["kill-server"]);
    assert!(e.until(5.0, || t.clients() == 0), "tmux clients left: {}", t.ok(&["list-clients"]));
    assert_eq!(t.snapshot(&["%2", "%3"]), untyped);
}

#[test]
fn bridge_failures_and_endings_never_hurt_tmux() {
    if !tmux_ok() {
        eprintln!("tmux not found; skipping");
        return;
    }
    let e = Env::new("bridge", None);
    let t = tmux_fixture(&e);
    t.ok(&["new-window", "-t", "work", "-n", "flood", "while read n; do seq 1 $n; echo SEQ-DONE-$n; done"]);
    let flood = t.ok(&["display", "-p", "-t", "work:flood", "#{pane_id}"]).trim().to_string();
    assert!(e.until(5.0, || t.ok(&["display", "-p", "-t", &flood, "#{pane_current_command}"]).trim() == "sh"));
    let out = e.ok(&["migrate", "-S", t.path(), "--apply"]);
    assert!(e.until(5.0, || t.clients() == 3), "expected 3 bridges ({out}), tmux clients:\n{}", t.ok(&["list-clients"]));
    // A bridge that dies takes only its own tmux client with it; the program
    // keeps running in tmux and still answers.
    let bridge = e.pids(&format!("__tmux-bridge -S {} -t %2 ", t.path()));
    assert_eq!(bridge.len(), 1, "{bridge:?}");
    unsafe {
        libc::kill(bridge[0], libc::SIGKILL);
    }
    assert!(e.until(5.0, || t.clients() == 2), "tmux clients: {}", t.clients());
    t.ok(&["send-keys", "-t", "%2", "still here", "Enter"]);
    assert!(e.until(5.0, || t.ok(&["capture-pane", "-p", "-t", "%2"]).matches("still here").count() == 2));
    // A flood stays bounded in tmux and catches up in amux.
    let tmux_pid: i32 = t.ok(&["display", "-p", "#{pid}"]).trim().parse().unwrap();
    let rss = || {
        std::fs::read_to_string(format!("/proc/{tmux_pid}/status"))
            .unwrap()
            .lines()
            .find(|l| l.starts_with("VmRSS"))
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse::<u64>()
            .unwrap()
    };
    e.ok(&["send-keys", "-t", "work:flood", "200000", "Enter"]);
    let mut peak = 0;
    assert!(
        e.until(30.0, || {
            peak = peak.max(rss());
            e.capture("work:flood").contains("SEQ-DONE-200000")
        }),
        "the flood never arrived"
    );
    assert!(peak < 64 * 1024, "tmux grew to {} MB", peak / 1024);
    // A live program that ends leaves an amux shell in its directory.
    e.ok(&["send-keys", "-t", "work:paste", "C-d"]);
    assert!(e.until(5.0, || e.capture("work:paste").contains("this is an amux shell now")), "{}", e.capture("work:paste"));
    let a = e.dir.join("a").to_str().unwrap().to_string();
    assert!(e.until(5.0, || e.display("work:paste", "#{pane_current_path}") == a));
    assert!(!t.ok(&["list-windows", "-t", "work", "-F", "#{window_name}"]).contains("paste"));
    assert!(e.until(5.0, || t.clients() == 1), "tmux clients: {}", t.clients());
    // The worst case: the only client left dies in the middle of a flood,
    // with output still on its way. tmux must neither keep it attached nor
    // stop reading the program (which would freeze it).
    e.ok(&["send-keys", "-t", "work:flood", "3000000", "Enter"]);
    assert!(e.until(10.0, || t.ok(&["capture-pane", "-p", "-t", &flood]).lines().any(|l| l.parse::<u64>().is_ok_and(|n| n > 1000))));
    let bridge = e.pids(&format!("__tmux-bridge -S {} -t {} ", t.path(), flood));
    assert_eq!(bridge.len(), 1, "{bridge:?}");
    unsafe {
        libc::kill(bridge[0], libc::SIGKILL);
    }
    assert!(e.until(5.0, || t.clients() == 0), "a dead bridge left a tmux client: {}", t.ok(&["list-clients"]));
    assert!(
        e.until(60.0, || t.ok(&["capture-pane", "-p", "-t", &flood]).contains("SEQ-DONE-3000000")),
        "the program stopped: {}",
        t.ok(&["capture-pane", "-p", "-t", &flood])
    );
    // A bridge only attaches to the pane it was made for: the same pane id,
    // and the same process, started at the same time (after a reboot, a new
    // tmux reuses pane ids).
    let pid = t.ok(&["display", "-p", "-t", &flood, "#{pane_pid}"]).trim().to_string();
    let imposter = format!("{} __tmux-bridge -S {} -t {} --pid {} --start 1", bin(), t.path(), flood, pid);
    e.ok(&["new-window", "-d", "-t", "work", "-n", "imposter", &imposter]);
    assert!(e.until(5.0, || e.capture("work:imposter").contains("this is an amux shell now")), "{}", e.capture("work:imposter"));
    assert_eq!(t.clients(), 0);
}

#[test]
fn migrate_keeps_panes_fresh_when_detaching_could_destroy_tmux_sessions() {
    if !tmux_ok() {
        eprintln!("tmux not found; skipping");
        return;
    }
    let e = Env::new("migratesafe", None);
    let t = tmux_fixture(&e);
    // Under both options tmux destroys things as soon as no client is
    // attached (it checks constantly): keep one attached while checking.
    let mut client = Command::new("tmux")
        .args(["-S", t.path(), "-C", "attach-session", "-t", "work"])
        .env_remove("TMUX")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    assert!(e.until(5.0, || t.clients() == 1));
    t.ok(&["set-option", "-t", "work", "destroy-unattached", "on"]);
    let plan = e.ok(&["migrate", "-S", t.path()]);
    assert!(plan.contains("%2    fresh cat") && plan.contains("destroy-unattached on"), "{plan}");
    t.ok(&["set-option", "-t", "work", "destroy-unattached", "off"]);
    t.ok(&["set-option", "-s", "exit-unattached", "on"]);
    let plan = e.ok(&["migrate", "-S", t.path()]);
    assert!(plan.contains("%2    fresh cat") && plan.contains("exit-unattached on"), "{plan}");
    t.ok(&["set-option", "-s", "exit-unattached", "off"]);
    let _ = client.kill();
    let _ = client.wait();
    let (code, _, err) = e.stderr(&["migrate", "-S", t.path(), "-t", "nosuch"]);
    assert_eq!(code, Some(1));
    assert!(err.contains("no session named nosuch"), "{err}");
    // Inside amux with `tmux-env on`, $TMUX names amux itself: not a tmux
    // server to read. (TMUX_TMPDIR keeps the fallback inside the test.)
    let fake = e.dir.join("amux.sock").to_string_lossy().into_owned();
    let o = e.cmd(&["migrate"]).env("TMUX", format!("{},1,0", fake)).env("AMUX", format!("{},1,0", fake)).env("TMUX_TMPDIR", &e.dir).output().unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(
        !o.status.success() && !err.contains(&fake) && err.contains(&e.dir.join(format!("tmux-{}", unsafe { libc::getuid() })).to_string_lossy().into_owned()),
        "{err}"
    );
}

#[test]
fn session_reattach_and_switching_work_across_cli_keys_and_tree() {
    if !tmux_ok() {
        eprintln!("tmux not found; skipping");
        return;
    }
    let e = Env::new("sessions", None);
    let sock = e.dir.join("reattach.sock").to_string_lossy().into_owned();
    let shell_cmd = format!(
        "env AMUX_RUNTIME_DIR={} AMUX_STATE_DIR={} AMUX_PLUGIN_DIR={} SHELL=/bin/sh PS1='OUTER$ ' /bin/sh -i",
        e.dir.join("run").display(),
        e.dir.join("state").display(),
        e.dir.join("plugins").display(),
    );
    let ok = Command::new("tmux")
        .args(["-S", &sock, "-f", "/dev/null", "new", "-d", "-s", "ui", "-x", "120", "-y", "40", &shell_cmd])
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .status();
    assert!(ok.is_ok_and(|s| s.success()), "cannot start outer tmux");
    let ui = Outer { sock };
    let amux = format!("{} -L {} -f {}", bin(), e.name, e.config);
    let send_line = |line: &str| {
        let _ = Command::new("tmux").args(["-S", &ui.sock, "send-keys", "-t", "ui", "-l", line]).env_remove("TMUX").status();
        ui.keys(&["Enter"]);
    };

    // 1. Create `alpha` interactively, detach (`prefix d`), then add `beta` with two windows.
    assert!(e.until(5.0, || ui.screen().contains("OUTER$")), "{}", ui.screen());
    send_line(&format!("{amux} new -s alpha"));
    assert!(e.until(5.0, || ui.screen().contains("[alpha]")), "{}", ui.screen());
    e.ok(&["send-keys", "-t", "alpha", "echo ALPHA-ONE", "Enter"]);
    assert!(e.until(5.0, || ui.screen().contains("ALPHA-ONE")), "{}", ui.screen());
    ui.keys(&["C-b", "d"]);
    assert!(e.until(5.0, || ui.screen().contains("[detached (from session alpha)]")), "{}", ui.screen());

    e.ok(&["new", "-d", "-s", "beta"]);
    e.ok(&["new-window", "-t", "beta", "-n", "two"]);
    e.ok(&["select-window", "-t", "beta:0"]);
    e.ok(&["send-keys", "-t", "beta:0", "echo BETA-WIN0", "Enter"]);
    e.ok(&["send-keys", "-t", "beta:1", "echo BETA-WIN1", "Enter"]);
    assert!(e.until(5.0, || e.capture("beta:0").contains("BETA-WIN0") && e.capture("beta:1").contains("BETA-WIN1")));

    // 2. Reattach by positional name (`amux a beta`), detach, and reattach to `beta:1` and via `new -A`.
    send_line(&format!("{amux} a beta"));
    assert!(e.until(5.0, || ui.screen().contains("[beta]") && ui.screen().contains("BETA-WIN0")), "{}", ui.screen());
    ui.keys(&["C-b", "d"]);
    assert!(e.until(5.0, || ui.screen().contains("[detached (from session beta)]")), "{}", ui.screen());

    send_line(&format!("{amux} attach -t beta:1"));
    assert!(e.until(5.0, || ui.screen().contains("[beta]") && ui.screen().contains("BETA-WIN1")), "{}", ui.screen());
    ui.keys(&["C-b", "d"]);
    assert!(e.until(5.0, || ui.screen().contains("[detached (from session beta)]")), "{}", ui.screen());

    send_line(&format!("{amux} new -A -s beta"));
    assert!(e.until(5.0, || ui.screen().contains("[beta]") && ui.screen().contains("BETA-WIN1")), "{}", ui.screen());
    assert_eq!(e.ok(&["ls"]).lines().count(), 2);

    // 3. `choose-tree` (`prefix s` / `prefix w`): live preview, `v` toggle, inline expand/collapse, and multi-pane split preview.
    e.ok(&["split-window", "-h", "-t", "beta:0"]);
    e.ok(&["send-keys", "-t", "beta:0.1", "echo BETA-SPLIT-RIGHT", "Enter"]);
    assert!(e.until(5.0, || e.capture("beta:0.1").contains("BETA-SPLIT-RIGHT")));

    ui.keys(&["C-b", "s"]);
    assert!(
        e.until(5.0, || ui.screen().contains("choose-tree") && ui.screen().contains("preview [beta]") && ui.screen().contains("BETA-WIN1")),
        "{}",
        ui.screen()
    );
    ui.keys(&["k"]);
    assert!(e.until(5.0, || ui.screen().contains("preview [alpha]") && ui.screen().contains("ALPHA-ONE")), "{}", ui.screen());
    ui.keys(&["v"]);
    assert!(e.until(5.0, || !ui.screen().contains("preview [alpha]") && !ui.screen().contains("ALPHA-ONE")), "{}", ui.screen());
    ui.keys(&["v"]);
    assert!(e.until(5.0, || ui.screen().contains("preview [alpha]") && ui.screen().contains("ALPHA-ONE")), "{}", ui.screen());
    // Expand `beta` inline with `l`, preview its split window `0` (plus live output updates) and window `1`, then collapse with `h`.
    ui.keys(&["j", "l", "j"]);
    assert!(
        e.until(5.0, || {
            let s = ui.screen();
            s.contains("(2 panes)") && s.contains("BETA-WIN0") && s.contains("BETA-SPLIT-RIGHT") && s.contains('│')
        }),
        "{}",
        ui.screen()
    );
    e.ok(&["send-keys", "-t", "beta:0.1", "echo LIVE-STREAM-77", "Enter"]);
    assert!(e.until(5.0, || ui.screen().contains("LIVE-STREAM-77")), "{}", ui.screen());
    ui.keys(&["j"]);
    assert!(e.until(5.0, || ui.screen().contains("BETA-WIN1")), "{}", ui.screen());
    ui.keys(&["h"]);
    assert!(e.until(5.0, || ui.screen().contains("▸ beta:") && !ui.screen().contains("(2 panes)")), "{}", ui.screen());
    ui.keys(&["k", "Enter"]);
    assert!(e.until(5.0, || ui.screen().contains("[alpha]") && ui.screen().contains("ALPHA-ONE")), "{}", ui.screen());

    ui.keys(&["C-b", "w"]);
    assert!(e.until(5.0, || ui.screen().contains("choose-tree")), "{}", ui.screen());
    ui.keys(&["j", "j", "Enter"]);
    assert!(e.until(5.0, || ui.screen().contains("[beta]") && ui.screen().contains("BETA-WIN0")), "{}", ui.screen());

    // 4. Switch via `prefix L` (last), `prefix )` (next), prompt `:switch-client -t $0`, and CLI `switch-client`.
    ui.keys(&["C-b", "L"]);
    assert!(e.until(5.0, || ui.screen().contains("[alpha]")), "{}", ui.screen());
    ui.keys(&["C-b", ")"]);
    assert!(e.until(5.0, || ui.screen().contains("[beta]")), "{}", ui.screen());
    ui.keys(&["C-b", ":"]);
    assert!(e.until(3.0, || ui.screen().contains(":")), "{}", ui.screen());
    send_line("switch-client -t $0");
    assert!(e.until(5.0, || ui.screen().contains("[alpha]")), "{}", ui.screen());
    e.ok(&["switch-client", "-n"]);
    assert!(e.until(5.0, || ui.screen().contains("[beta]")), "{}", ui.screen());
    e.ok(&["switch-client", "-l"]);
    assert!(e.until(5.0, || ui.screen().contains("[alpha]")), "{}", ui.screen());
    e.ok(&["switch-client", "beta"]);
    assert!(e.until(5.0, || ui.screen().contains("[beta]")), "{}", ui.screen());

    // 5. Running `amux a alpha` and `amux new -s gamma` inside an amux pane switches the outer client cleanly.
    send_line(&format!("{amux} a alpha"));
    assert!(e.until(5.0, || ui.screen().contains("[alpha]") && ui.screen().contains("ALPHA-ONE")), "{}", ui.screen());
    send_line(&format!("{amux} new -s gamma"));
    assert!(e.until(5.0, || ui.screen().contains("[gamma]")), "{}", ui.screen());
    assert_eq!(e.ok(&["display", "-p", "-t", "gamma", "#{window_height}"]).trim(), "39");

    // 6. `x` in `choose-tree -s` kills the selected session (`beta`), not the current one (`gamma`).
    ui.keys(&["C-b", "s"]);
    assert!(e.until(5.0, || ui.screen().contains("choose-tree")), "{}", ui.screen());
    ui.keys(&["k", "x"]);
    assert!(e.until(5.0, || ui.screen().contains("[gamma]") && !e.ok(&["ls"]).contains("beta:")), "{}", ui.screen());

    // 7. `restart-server` and server crashes reconnect to the switched-to session (`gamma`), not `beta`.
    let old_srv = e.pids(&format!("__server {}", e.name));
    e.ok(&["restart-server"]);
    assert!(e.until(10.0, || ui.screen().contains("[gamma]") && e.pids(&format!("__server {}", e.name)) != old_srv), "{}", ui.screen());
    let _ = e.run(&["panic-test"]);
    assert!(e.until(10.0, || ui.screen().contains("[gamma]")), "{}", ui.screen());
}
