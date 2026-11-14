# amux plugins

Extend amux while it runs. A plugin is a small program, in any language, that
talks to amux with JSON-RPC over its stdin and stdout. Save a file in the plugin
directory and amux loads it within a second: no recompiling, no reinstalling, no
restart. Save it again and it hot-reloads.

Everything a plugin registers is an *effect owned by the plugin*: commands, key
bindings, event subscriptions, status-line text, agent-dashboard annotations,
agent statuses and agent definitions. amux undoes all of it when the plugin
stops, reloads or crashes.
A plugin runs as a separate process, so a crashing plugin never takes amux or
your panes down.

## Quick start (for people and coding agents)

    amux plugin new hello          # creates a working Python plugin and loads it
    amux hello world               # run its command
    $EDITOR "$(amux plugin dir)/hello.py"
    amux plugin reload hello       # reload now; prints whether it came up
    amux plugin show hello         # state, registrations, last error
    amux plugin logs hello         # its stderr: print() output and tracebacks
    amux plugin trace hello        # protocol messages between amux and the plugin
    amux plugin view               # (or prefix P) all plugins, live logs and traces

When asked to add a feature to amux, build it as a plugin:

1. **Start small.** Pick the smallest version that works and create it with
   `amux plugin new NAME`. If the feature needs an event, check the event table
   below first.
2. **Edit the file.** Every save reloads the plugin. Run
   `amux plugin reload NAME` to reload synchronously: it exits with status 1 and
   prints the error if the plugin did not come up.
3. **Verify by using it.**
   - Run its command (`amux NAME ...`), or simulate an event with
     `amux plugin emit EVENT -t PANE key=value...`.
   - `amux plugin show NAME` must list the commands, keys and events you expect.
   - `amux plugin logs NAME` has tracebacks. If a handler never runs,
     `amux plugin trace NAME` shows whether the event reached the plugin.
4. **Fix and report.** Fix what you observed, then report the file path, how to
   use the feature, and anything you could not verify.

Rules of thumb:
- Do not edit other plugins unless asked.
- Commands must answer within 15 seconds. Do slow work in `@amux.every` timers.
- Never put secrets in plugin output or logs.

## Files and environment

- **Directory.** `$AMUX_PLUGIN_DIR`, else `$XDG_CONFIG_HOME/amux/plugins` (by
  default `~/.config/amux/plugins`). Print it with `amux plugin dir`.
- **What counts as a plugin.** A file such as `name.py`, `name.sh`, `name.js`,
  `name.rb` or `name.pl`; any file starting with a `#!` line; or any executable.
  A directory `name/` also works if it contains `main.*`, `plugin.*`, `index.*`
  or an executable `run`. Hidden files, editor backups and `*.md`/`*.json`/`*.txt`
  files are ignored.
- **Install and remove.** `amux plugin install PATH` copies a plugin file or
  directory into the plugin directory and loads it. `amux plugin remove NAME`
  moves it to amux's state directory.
- **Turning off.** `amux plugin disable NAME` and `amux plugin enable NAME` are
  persistent. `set -g plugins off` turns the whole system off.
- **Process environment.** A plugin runs in its own directory and gets:
  - `AMUX_PLUGIN`: its name.
  - `AMUX_PLUGIN_DATA`: a private directory that survives reloads.
  - `AMUX_PLUGIN_LOG` and `AMUX_SERVER`.
  - `amux` and `tmux` on `PATH`, both talking to this server.
  - For Python, `PYTHONPATH` already includes the SDK.
- **Permissions.** amux refuses to run world-writable files and files you do
  not own.

## Debugging

- **Plugin view.** Press `prefix P`, or run `amux plugin view [NAME]` from a
  shell inside amux. It lists every plugin with its state; plugins that failed
  or need attention come first. Below the list are the selected plugin's
  details and the live end of its log.
  - `t` switches between the log, the protocol trace and the details.
  - `enter` shows them full screen: scroll with `j`/`k`, PgUp/PgDn and `g`/`G`
    (`G` follows new output again).
  - `r` reloads the plugin and `d` disables or enables it.
  - `e` opens the file in `$EDITOR` in a new window; saving reloads it.
  - `x` removes the plugin and `q` closes the view.
- **`amux plugin logs NAME [-n N]`** prints stderr: `print()` output and
  tracebacks. Each start begins with a `--- TIME starting:` line.
- **`amux plugin trace NAME [-n N]`** prints the last protocol messages, up to
  200. `→` is amux to the plugin, `←` the plugin to amux, `✗` a message not
  delivered (the plugin was not reading) and `·` a start, exit or failure.
- **`amux plugin show NAME`** prints the state, registrations, rejected names
  and last error.

## Lifecycle

| Situation | What amux does |
|---|---|
| Plugin registers, or stays alive for 1.5 s | **running**. Attached clients see "plugin NAME loaded". |
| File saved | Stops the old process: sends `shutdown` and closes stdin, then SIGTERM at 0.3 s and SIGKILL at 1.3 s. Then starts the new one. |
| Exits before it is running | **failed**. Stays off until the file changes or `amux plugin reload`; no restart loops. |
| Crashes later | Restarted with backoff (1, 2, 4… s). 5 crashes in 5 minutes means **failed**. |
| Exits with status 0 | **exited** (one-shot plugins are fine). |
| amux stops | Sends `shutdown` and closes stdin, then SIGTERM at 0.15 s and SIGKILL at 0.4 s, to the whole process group. |
| amux crashes | Plugins die with it. The restarted server kills anything their processes left behind, then starts them again. |

A plugin can be stopped at any moment, so keep state in `AMUX_PLUGIN_DATA` and
write it as you go (write a temporary file, then rename it) rather than at exit.

## Python SDK

The `amux_plugin` module is on `PYTHONPATH` for every plugin.

```python
import amux_plugin as amux

@amux.command("hello", help="Say hello")
def hello(ctx, *args):
    # ctx.pane, ctx.session, ctx.window, ctx.cwd, ctx.client: the caller
    return "hello " + " ".join(args)        # str, list of lines, dict or None

@amux.on("agent-state")
def on_state(ev):
    if ev.state == "waiting":
        amux.notify(f"{ev.agent} in {ev.pane} needs you: {ev.summary}")

@amux.every(30)
def tick():
    busy = [a for a in amux.agents() if a.state == "working"]
    amux.status(f"#[fg=green]⚙{len(busy)}" if busy else "")

amux.key("H", "hello", table="prefix")    # prefix H runs `hello`
amux.main()                               # last line: serve requests forever
```

| API | Purpose |
|---|---|
| `@command(name, help="")` | Add a command, usable as `amux NAME`, from `prefix :` and in key bindings |
| `@on(event, ...)` | Handle events (`"*"` means every event except `output`) |
| `@every(seconds)` | Run a function periodically |
| `key(key, command, table="prefix")` | Bind a key; the previous binding is restored on unload |
| `agent(name, cmd, working=[], waiting=[], chrome=[], resume=None)` | Teach amux to recognise a coding agent CLI (see below) |
| `run("tmux command string")` / `run(*argv)` | Run any amux/tmux command. Returns stdout; raises `AmuxError` on failure. Inside a command handler it targets the caller's pane. |
| `run_json(...)` | `run()`, then parse the JSON |
| `agents()` | Agents as objects (`pane`, `agent`, `state`, `summary`, `topic`, `cwd`, `session`, `window`) |
| `status(text)` | Text in the status line (tmux `#[fg=red]` styles allowed); `""` clears |
| `annotate(pane, text)` | Short tag on an agent in the dashboard and `amux agents`; `""` clears |
| `notify(message, pane=None)` | Desktop notification, plus a message on attached clients |
| `log(...)` | Write to the plugin log. `print()` goes there too, because stdout is the protocol. |
| `name`, `data_dir`, `AmuxError` | Plugin name, private data directory, error type |

Event and context objects are dicts whose keys also read as attributes, and a
missing key reads as `None`. A plugin can be configured with tmux user options:
`amux.run("show-options -gqv @my-setting")`.

## Events

Every event carries `event` and `time`. Pane events also carry the pane's
details:
- `pane` (like `"%3"`), `session`, `window`, `cwd`, `command`, `title`;
- `agent`, `state`, `summary` and `topic`, when the pane runs a coding agent.

| Event | When | Extra fields |
|---|---|---|
| `agent-state` | An agent's state changes (`working`, `waiting`, `done`, `idle`, `exited`) | `prev` |
| `notify` | A program in a pane sends a notification (OSC 9/777) | `message` |
| `bell` | Bell in a pane | |
| `exit` | A pane's program exits | `code` |
| `command-done` | A shell command finishes (OSC 133 prompt marks) | `code` |
| `output` | A pane's screen changes, at most 4 times a second per pane | `lines`: last 5 non-empty lines |
| `pane-new`, `pane-closed` | A pane is created or destroyed | |
| `client-attached`, `client-detached` | A terminal attaches or leaves | `client`, `session` |

To test a handler, run `amux plugin emit agent-state -t %3 state=waiting`.

## Teach amux about an agent

amux recognises a coding agent from its command line, then reads its state from
the screen. Built-in definitions cover common agent CLIs; `amux agents --kinds`
lists the definitions in use and where each comes from. A plugin can add one,
or replace the built-in definition of the same name, for example after an
agent CLI changes its prompts:

```python
amux.agent("kiro",
    cmd=r"(^|[/ ])kiro(-cli)?( |$)",     # matched against the command line
    working=[r"thinking"],               # screen lines meaning "working"
    waiting=[r"allow this action\?"],    # lines meaning "needs the user"
    chrome=[r"type a message"],          # lines never used as the summary
    resume="kiro-cli chat --resume")     # typed into a shell after a reboot
```

- `cmd` is a regular expression matched against the program and its first
  argument, so `node /path/to/kiro` matches too. It must not match a plain
  shell.
- `working`, `waiting` and `chrome` are case-insensitive regular expressions.
  They are matched against the bottom lines of the screen, newest first.
  Patterns most agent TUIs share (`esc to interrupt`, `(y/n)`,
  `do you want to proceed`…) are always included.
- Panes already running the agent are recognised at once. Check with
  `amux agents --kinds` and `amux agents`.
- When the plugin unloads, panes fall back to the built-in definitions.
- If the agent CLI has hooks, `amux status set` from them is exact and beats
  any screen pattern.

## Commands worth knowing for `run()`

- `send-keys -t %3 -l 'text'` then `send-keys -t %3 Enter`: type into a pane.
- `display-message 'text'`: show a message to attached clients.
- `new-window -n NAME -c DIR 'cmd'` and `split-window -h -c DIR 'cmd'`: open
  panes.
- `capture-pane -p -t %3` (add `-S -` for all history): read a pane.
- `agents --json`, `list-panes -a -F '#{pane_id} #{pane_current_path}'`: inspect
  agents and panes.
- `status set -t %3 waiting 'reason'`: set an agent's state. Undone when the
  plugin unloads.
- `set -g @name value`: store a value, then use `#{@name}` in formats.

## Protocol (any language)

The protocol is one JSON object per line: newline-delimited JSON-RPC 2.0, where
the `"jsonrpc"` field is optional. stdout carries protocol messages only; stderr
goes to the plugin log. Exit when stdin closes.

Plugin to amux (requests have an `id` and get a reply; notifications have none):

```
{"id":1,"method":"register","params":{"commands":[{"name":"x","help":"..."}],
   "keys":[{"key":"g","command":"x","table":"prefix"}],"events":["agent-state"],
   "agents":[{"name":"kiro","cmd":"kiro","waiting":["allow\\?"],"resume":"kiro --resume"}]}}
   -> {"jsonrpc":"2.0","id":1,"result":{"commands":["x"],"keys":["prefix g"],"events":["agent-state"],"agents":["kiro"],"rejected":[]}}
{"id":2,"method":"run","params":{"command":"display-message -p '#{session_name}'","pane":"%3"}}
   -> {"jsonrpc":"2.0","id":2,"result":{"stdout":"main\n","stderr":"","code":0}}   ("argv":[...] also works)
{"method":"status","params":{"text":"..."}}           {"method":"annotate","params":{"pane":"%3","text":"..."}}
{"method":"notify","params":{"message":"..."}}        {"method":"log","params":{"message":"..."}}
```

Calling `register` again replaces the previous commands, keys, events and agent
definitions.

amux to plugin:

```
{"jsonrpc":"2.0","method":"hello","params":{"api":1,"name":"x","amux":"0.1.0",...}}     on start
{"jsonrpc":"2.0","id":7,"method":"command","params":{"name":"x","args":["a"],"pane":"%3","session":"main",...}}
   reply {"id":7,"result":"text"} (or a list of lines), or {"id":7,"error":{"code":1,"message":"..."}}
{"jsonrpc":"2.0","method":"event","params":{"event":"agent-state","pane":"%3","state":"waiting",...}}
{"jsonrpc":"2.0","method":"shutdown","params":{}}                                        before a reload or stop
```

A complete plugin in POSIX sh (`amux plugin new NAME --lang sh` creates one):

```sh
#!/bin/sh
echo '{"id":0,"method":"register","params":{"commands":[{"name":"hi","help":"say hi"}]}}'
while IFS= read -r line; do
  case $line in *'"method":"command"'*)
    id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9]*\).*/\1/p')
    echo "{\"id\":$id,\"result\":\"hi from sh\"}" ;;
  esac
done
```
