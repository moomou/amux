# amux

A terminal multiplexer for the agentic age: **crash-proof**, **agent-aware**,
**fast**, **extensible by your agents while it runs**, and a drop-in for tmux
(your `~/.tmux.conf`, key bindings, pane splits and session commands keep
working).

> Status: experimental. Linux only. Verified against tmux 3.2a.

```sh
cargo install --git https://github.com/moomou/amux

amux                 # new session (reads ~/.tmux.conf)
amux a [NAME]        # reattach (or switch if already inside amux)
```

Inside amux: `prefix A` opens the **agent dashboard**, `prefix N` jumps to the
next agent that needs you and `prefix P` opens the **plugin view**. Everything
else is tmux. To add a feature, ask an agent to build it as a plugin; it goes
live the moment the file is saved (section 5). Already working in tmux?
`amux migrate` moves your running sessions over without changing tmux
(section 6).

---

## 1. Crash-proof by construction

tmux keeps every pane's terminal state inside one server process: if it
crashes, every program in every pane dies with it. amux splits that state up.

```
  terminal ── amux client ──┐ (reconnects automatically)
                            │
                   amux server (compositor, layout, commands, agents)
                    │   restarted by the supervisor; state in state.json
        ┌───────────┼───────────┐
     keeper      keeper      keeper     one tiny process per pane: owns the PTY,
        │           │           │       the terminal emulator and scrollback
      shell      claude       vim
```

| Failure | What happens | Panes survive? |
|---|---|---|
| Server crash / `kill -9` / hang | Supervisor restarts it (watchdog catches hangs). The new server reconnects to every live keeper; attached clients show a notice and reattach. | Yes: programs and screens are untouched |
| Bug in a pane's terminal emulator | The keeper catches the panic, rebuilds the emulator from its last good state and asks the app to redraw. | Yes |
| Keeper killed | The pane is respawned in place (same layout, cwd and scrollback) with a banner. | Program restarted |
| Reboot / everything killed | First `amux` command resurrects all sessions, windows, splits, cwd and scrollback. Agents are offered a resume (`claude --continue` is typed at the prompt, not run). | Layout and history restored |
| Server state file lost | Running keepers are adopted into a `recovered` session. | Yes |

How:
- **Crash-only server.** Layout and pane metadata are written atomically and
  fsync'd after every structural change. Restarting from `state.json` is the
  only recovery path, so it is always exercised.
- **Keepers outlive the server.** They hold the PTY master and keep draining
  output while no server is connected, so programs never block. They answer
  terminal queries (DA, DSR, OSC 10/11, DECRQM) themselves.
- **Scrollback snapshots.** Every `snapshot-interval` seconds (default 30),
  panes with new output save compact snapshots for reboot resurrection.

Try it yourself: `amux panic-test` crashes the server, and
`amux panic-test -k -t %3` crashes one pane's emulator.

## 2. The agent view

amux recognises coding agents running in panes, including Claude Code, Codex,
Copilot CLI, Gemini, Qwen, OpenCode, pi, Aider, Goose, Amp, cursor-agent and
Crush. These definitions are data: a plugin can add an agent CLI, or fix the
patterns of a built-in one, while amux runs (`amux agents --kinds` lists them;
see section 5). amux tracks what each agent is doing:

| State | Meaning |
|---|---|
| ▲ waiting | Needs you: an approval prompt or a question |
| ● working | Spinner or streaming output |
| ✓ done | Finished its turn; you haven't looked yet |
| ○ idle | Nothing happening |

- **Dashboard** (`prefix A`): every agent across all sessions, sorted by
  urgency. Each row shows the topic (the terminal title), the latest line of
  output or the pending question, time in state, and a 60 s activity
  sparkline. A live preview of the selected agent sits below the list.
  - Keys: `enter`/`1-9` jump, `r` reply without leaving (e.g. type `1` to
    approve), `t` tiles view, `/` filter, `x` kill, `q` back.
- **Attention routing.**
  - `prefix N` cycles through agents that need you.
  - The status line shows a badge (`▲1 ●2 ✓1`).
  - When an agent you can't see starts waiting or finishes, amux sends a
    desktop notification (OSC 9) to your terminal.
- **CLI.** `amux agents` gives a table, `--json` gives machine-readable
  output and `--watch` streams changes.

Detection uses generic signals, in priority order:
1. Explicit status from hooks.
2. OSC 9/777 notifications and OSC 9;4 progress.
3. Bells.
4. Screen patterns for approval prompts and spinners.
5. Output activity.

Input boxes, key hints and status bars are ignored when summarising.
Answering a prompt (typing in the pane or replying from the dashboard)
switches the agent back to working at once.

**Explicit status (hooks).** Any program in a pane can report its state. For
example, these Claude Code hooks in `~/.claude/settings.json` make the state
exact:

```json
{ "hooks": {
  "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "amux status set working" }] }],
  "Notification":     [{ "hooks": [{ "type": "command", "command": "amux status set waiting \"$(jq -r .message)\"" }] }],
  "Stop":             [{ "hooks": [{ "type": "command", "command": "amux status set done" }] }]
} }
```

`amux status set working|waiting|done|idle [message]` and `amux status clear`
target the calling pane through `$AMUX_PANE`. Any pane can be marked this way,
even a plain build script. A mark set by a program running in the pane (a
hook or a script) lasts while that program runs; one set from elsewhere lasts
until `amux status clear`.

## 3. Performance

Rust, `poll`-driven, no threads, no async runtime.
- **Compact cells:** 12-byte cells, with history packed as style runs
  (about 1 byte per character instead of tmux's 4 to 8).
- **Incremental output:** keepers send only dirty rows, with trailing blanks
  trimmed. The server keeps sparse pane mirrors, caches the expanded status
  line, and diffs each frame against what the client already shows (with
  memcmp row skips), wrapped in synchronized-output markers.
- **Fast emulator:** ASCII runs are written in bulk, and blocks are trimmed
  per row, not per line.

`bench/bench.py`, 10 panes at 200×50, medians of 3 alternating runs on a busy
12-core host (load average ~15). Only the multiplexer's own processes are
counted: tmux server + client vs amux supervisor + server + keepers + client.

| Metric | tmux 3.2a | amux | |
|---|---|---|---|
| Output throughput (`cat` 40 MB in the visible pane) | 30.9 MB/s | **108 MB/s** | 3.5× faster |
| Multiplexer CPU for that `cat` | 1008 ms | **299 ms** | 3.4× less |
| Memory, 10 panes with 10k lines of scrollback each (PSS) | 53.6 MB | **20.2 MB** | 2.7× less |
| Agent-style spinner, 10 Hz redraw, CPU over 10 s | 26.9 ms | **22.7 ms** | 16% less |
| Idle CPU over 10 s, client attached | 0.5 ms | 0.4 ms | same |
| Memory, 10 idle shells, no scrollback (PSS) | **1.4 MB** | 6.8 MB | one process per pane |

- **Throughput and CPU:** output is parsed by the pane's keeper and only
  screen changes travel further, so amux drains a fast producer 3.5× faster
  than tmux using less than a third of the CPU.
- **Memory:** scrollback is stored about 2.7× more compactly than tmux. The
  price of crash isolation is a fixed ~0.5 MB per pane for its keeper
  process, so ten *empty* shells cost more than in tmux. Any real scrollback
  outweighs it.
- **Agent workload:** a 10 Hz redraw of one status line (an agent "thinking")
  costs slightly less than in tmux, even though each update crosses three
  processes.

The emulator alone (`amux __bench-emu FILE`) parses typical `\r\n` text at
~190 MB/s.

## 4. Agentic patterns

Implemented:
- **Attention inbox.** Agents are triaged by urgency (waiting > working >
  done > idle) instead of by window index. `prefix N` is "next thing that
  needs me".
- **Reply in place.** Answer an approval from the dashboard without switching
  context.
- **Status protocol for hooks.** `amux status set …` turns any agent or
  script into a first-class citizen. Exact state beats screen-scraping.
- **Standard terminal signals.** OSC 9 / 777 notifications, OSC 9;4
  progress, OSC 133 prompt marks (a `command-done` event with the exit
  status) and OSC 7 cwd all feed agent state and events.
- **Automation API.**
  - `amux exec -- cmd` runs a command in a new window, waits, prints its
    output and returns its exit code.
  - `amux await -t %3 --pattern 'tests? passed' --timeout 600` (also `--idle
    5`, `--exit`, `--state waiting,done`).
  - `amux capture -t %3` prints full scrollback.
  - `amux events` streams JSON events (`agent-state`, `notify`, `bell`,
    `exit`, `command-done`).

  Together these let one agent orchestrate others in panes.
- **Durable agent sessions.** After a reboot, agent panes come back in the
  same directory with their scrollback and the agent's resume command typed
  at the prompt. The `agent-resume` option sets this to `typed` (default),
  `run` or `off`.
- **Crash containment.** A misbehaving TUI (escape-sequence fuzzing, huge
  outputs) can at worst restart its own pane's emulator.
- **Self-extension.** Agents add features to amux as hot-reloaded plugins and
  verify them live (section 5).

Available as example plugins (`examples/plugins/`):
- **Worktree per agent:** `amux worktree fix-login claude` creates a git
  worktree and branch, starts the agent there, and shows the branch and diff
  stat in the dashboard. Parallel agents never edit the same checkout.
- **Broadcast prompts:** `amux broadcast -f api "rebase on main"` sends one
  instruction to every waiting, done or idle agent (or those matching a
  filter).
- **Stuck detection:** agents that stay `working` with no real screen change
  for `@stuck-minutes` get a `‹stuck? 7m›` tag and a notification. Spinners
  and elapsed-time counters do not count as progress.

Worth adding next (most can be plugins):
- **Transcripts and audit log:** append-only per-pane logs (keepers already
  see every byte), searchable across sessions and sizes.
- **Budgets:** per-pane CPU, memory and time limits.
- **Remote or phone view:** a read-only web dashboard of agent states over
  the same event stream. Approvals from anywhere.
- **Cost meters:** parse token and cost lines that agents print, aggregated
  per session.
- **Side-by-side comparison:** a tiles view of N agents given the same
  prompt.

## 5. Plugins: agents extend amux while it runs

Inspired by [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness),
where everything is a hot-reloadable plugin and agents write new ones. Ask a
coding agent for a feature and it ships as a plugin: no recompiling, no
reinstalling, no restart.

```sh
amux plugin new hello            # a working Python plugin, already loaded
amux hello world                 # run its command
amux plugin guide                # the full guide: give it to your agent
```

Tell an agent: *"Read `amux plugin guide`, then build an amux plugin that …"*.
The guide ([docs/plugins.md](docs/plugins.md)) walks it through the loop:
1. Create the plugin with `amux plugin new`.
2. Edit it; every save hot-reloads it.
3. Verify by using the live feature. Run `amux NAME …`, or simulate an event
   with `amux plugin emit EVENT -t PANE key=value…`.
4. Check `amux plugin show NAME`, `amux plugin logs NAME` and, when an event
   seems to go missing, `amux plugin trace NAME`. People can watch all of it
   live in the plugin view (`prefix P`).

How it works:
- **Any language.** A plugin is a program in `~/.config/amux/plugins/`, either
  a file or a directory. It speaks JSON-RPC 2.0 over stdin/stdout, one object
  per line. A Python SDK is built in (`import amux_plugin as amux`), and a
  POSIX sh template shows the raw protocol.
- **Everything is an effect the plugin owns.** A plugin can add:
  - commands (`amux NAME …`, also usable from key bindings and
    `command-prompt`);
  - key bindings;
  - event handlers (agent state changes, notifications, bells, exits, command
    completion, pane output, panes and clients coming and going);
  - status-line text, dashboard annotations and agent statuses;
  - agent definitions: how to recognise an agent CLI and read its screen.

  amux undoes all of these when the plugin stops, reloads or crashes, and
  restores any key binding the plugin replaced.
- **Hot reload.**
  - Changes are picked up via inotify with a 150 ms debounce.
  - The old process is stopped (`shutdown`, then SIGTERM, then SIGKILL to its
    process group) before the new one starts.
  - A plugin that fails to start stays *failed*, with its traceback in
    `amux plugin show`, until the file changes. It does not restart-loop.
  - `amux plugin reload NAME` reloads synchronously and exits with status 1
    if the plugin did not come up or a registration was rejected.
- **Contained.**
  - Plugins are separate processes with non-blocking IO and a deadline on
    every request (15 s per command). A plugin that crashes, hangs or floods
    stdout cannot block amux or your panes.
  - Crashes restart with backoff.
  - Plugins cannot attach clients or take over the terminal.
  - Plugin processes die with the server. After a server crash, the new
    server cleans up anything they left behind and starts them again.
- **No cost when unused.** With no plugins, the host is one idle inotify
  descriptor. `set -g plugins off` removes even that.

| Command | Purpose |
|---|---|
| `amux plugin view [NAME]` (`prefix P`) | Plugin view: states, live logs, protocol traces; reload, disable, edit or remove |
| `amux plugin list` / `show NAME` / `logs NAME [-n N]` | State, registrations, last error, stderr |
| `amux plugin trace NAME [-n N]` | Recent protocol messages between amux and the plugin |
| `amux plugin new NAME [-l python\|sh]` | Create a working plugin from a template and load it |
| `amux plugin reload [NAME…]` | Reload now; exit status 1 if one did not come up |
| `amux plugin install PATH [-f]` / `remove NAME` | Copy a plugin in; move one out (kept under the state directory) |
| `amux plugin enable NAME` / `disable NAME` | Persistent on/off |
| `amux plugin emit EVENT [-t PANE] [KEY=VALUE…]` | Send a test event to subscribed plugins |
| `amux plugin dir` / `guide` | Print the plugin directory / the plugin guide |

Example plugins (install with `amux plugin install examples/plugins/NAME.py`):
`worktree.py`, `broadcast.py` and `stuck.py` (section 4).

## 6. tmux compatibility

amux reads `~/.tmux.conf` (or `-f FILE`) with tmux's command language:
- quoting, `\;` chains, `%if`, `#{formats}`, `#(shell)`, `#[styles]`, `bind
  -n/-r/-T`;
- `set -g/-w/-a/-u/-q`, `source-file`, `run-shell` and `if-shell`.

Inside `run-shell`, scripts that call `tmux …` talk to amux through a `tmux`
shim on their `PATH`. Your prefix, copy-mode keys (vi and emacs tables) and
split bindings work as before.

| Area | Status |
|---|---|
| Sessions | new/attach/detach/kill/rename/switch/has/ls, `-t` targets (`=name`, `$id`, `@id`, `%id`, `:idx.pane`, `{last}`…) |
| Windows | new/kill/select/next/prev/last/rename/move/swap/rotate/find, automatic-rename, activity/bell flags |
| Panes | split -h/-v/-b/-l/-p/-c, select (incl. -L/-R/-U/-D), resize/zoom, swap, break/join/move, respawn, kill, display-panes |
| Layouts | even-horizontal/vertical, main-horizontal/vertical, tiled, next/previous-layout |
| Copy mode | vi and emacs keys, search, selection (incl. rectangle), copy-pipe, buffers, OSC 52 clipboard |
| Mouse | click to focus, wheel scroll and copy mode, drag selection, border-drag resize, status-line window click |
| Status line | status-left/right/style/interval, window-status formats and styles, messages, command prompt |
| Formats | the common variables (`session_*`, `window_*`, `pane_*` incl. geometry, `client_*`, mode flags such as `cursor_flag`, conditionals, `t:`, `=N:`, `s///`) |
| Options | all common session/window/server options; unknown options are rejected like tmux (silenced with `-q`) |

Known gaps: `pipe-pane`, control mode (`-CC`), `display-popup`,
`display-menu`, hooks (`set-hook` is accepted but not run), linked windows,
and layout strings in `select-layout`.

Emulator fidelity is checked against real tmux. `tests/oracle_tmux.rs` feeds
random escape-sequence streams to both and compares screen, cursor and
history:

| Oracle check | Result |
|---|---|
| Parsing | 200/200 |
| Resize, shell-style content | 100/100 |
| Resize, full-feature streams | ~72% (alt screen, scroll regions and origin mode combined with resize differ) |

By default amux does not set `$TMUX` in panes (set `tmux-env on` if a tool
insists). Panes get `$AMUX` and `$AMUX_PANE` instead.

### Moving running tmux sessions into amux

```sh
amux migrate                # in tmux: print the plan (nothing changes)
amux migrate -t work        # just one session (repeat -t for more)
amux migrate --apply        # do it
amux attach -t work         # then attach from a terminal outside tmux
```

Each tmux session becomes an amux session with the same windows (names and
indexes), layouts, pane sizes and directories. If the name is taken, it
becomes `NAME-tmux`. Each pane moves in one of two ways:
- **live**: anything but a shell at its prompt (an agent, an editor, a
  server, ssh). The program keeps running in tmux; amux shows it and sends
  it what you type. When the tmux pane closes, the amux pane becomes an amux
  shell in its last directory.
- **fresh**: a shell at its prompt. A new amux shell starts in the same
  directory, below the pane's last 5000 lines of tmux history. Agents amux
  knows how to resume are resumed as set by `agent-resume`.

`--fresh` makes every pane fresh and `--live` every pane live. A pane that
had already exited keeps only its history.

**tmux is never changed.** What protects it (checked against tmux 3.2a):
- **Planning only reads.** Planning runs read-only tmux commands, from an
  allowlist: `list-*`, `display-message -p`, `capture-pane -p`,
  `show-options` and `show-hooks`. Without `--apply`, nothing starts or
  changes anywhere.
- **One control client per live pane.** Each live pane is shown by one tmux
  control-mode client (`amux __tmux-bridge`). It attaches to the pane's
  session, never to the pane, which would switch windows in tmux. It uses:
  - `-E`, so attaching never copies amux's environment into the session
    (`update-environment`, for example `SSH_AUTH_SOCK`);
  - `ignore-size` and no size of its own, so tmux never resizes for it;
  - `pause-after`, so tmux pauses its output instead of buffering without
    limit;
  - `-N`, so it never starts a tmux server (after a reboot, for example).

  Output from the session's other panes is turned off for it.
- **It never holds tmux back.** The bridge reads tmux as fast as tmux
  writes. Output amux cannot take yet is dropped and the screen repainted,
  so a flood leaves tmux's memory flat (5–6 MB measured).
- **Its tmux client dies with it** (`PR_SET_PDEATHSIG`). Otherwise tmux
  would keep an orphaned control client with unsent output attached
  forever. If that were the only client, tmux would stop reading the pane
  and the program would freeze. The tests reproduce this and check the fix.
- **It only attaches to the pane it was made for**: the same process,
  started at the same time. A restarted tmux reuses pane ids.
- **Input goes through tmux commands.** Keys go through `send-keys -H`.
  Pastes go through a uniquely named tmux buffer that is deleted again, and
  tmux brackets them only if the program asked.
- **Unsafe cases start fresh.** A pane starts fresh instead of live when
  amux detaching could destroy something: tmux older than 3.2,
  `exit-unattached on`, or `destroy-unattached on` in that session. The plan
  also lists hooks that run when a client attaches (`client-attached` and
  the like), because they will run for amux too.
- **No feedback loops.** The pane that runs `amux migrate`, and any pane
  running amux, starts fresh. amux also refuses to attach from a tmux pane
  that it shows live: amux would draw itself inside itself, and every key
  would go round between the two forever.
- **Stopping amux leaves tmux running.** Killing a bridge, or stopping
  amux, removes only amux's tmux clients; tmux and its programs carry on.

`tests/e2e.rs` snapshots tmux before migrating and after stopping amux, and
the snapshots must match. A snapshot covers sessions, windows, layouts,
processes, directories, buffers, session environments and the text of every
pane. The tests also kill bridges, including mid-flood, start a bridge
aimed at the wrong process, and try the refusals.

Limits:
- **Programs stay in tmux.** A live program keeps running inside tmux.
  Moving its terminal itself would need ptrace rights over tmux, which most
  Linux systems reserve for root (Yama `ptrace_scope`). Once the programs
  you care about are done, close tmux; each live pane then becomes an amux
  shell.
- **Size mismatch.** amux shows a live pane at the size of its tmux pane and
  never resizes tmux. If the amux pane is a different size, full-screen
  programs can look off.
- **Stopped bridges.** A bridge stopped with SIGSTOP stops reading tmux. If
  no other tmux client is attached, the programs it shows wait until it
  continues.
- **Reboots.** After a reboot (or a tmux restart), live panes come back as
  amux shells.

## 7. Reference

amux-only commands (tmux commands are not repeated here):

| Command | Purpose |
|---|---|
| `amux agents [--json] [--watch]` | List agents: pane, state, agent, location, time in state, `[topic] summary` |
| `amux agents --kinds [--json]` | Agent definitions in use (built in or from plugins) |
| `amux status set STATE [MSG]` / `status clear` / `status` | Report or show a pane's agent status (`-t` or `$AMUX_PANE`) |
| `amux exec [-d] [-n NAME] [-c DIR] [-T SECS] [-k] -- CMD…` | Run in a new window, wait, print output, exit with its status (`-k` keeps the window) |
| `amux await -t PANE (--pattern RE \| --idle SECS \| --exit \| --state LIST) [--timeout SECS]` | Block until a condition holds (exit 124 on timeout) |
| `amux capture -t PANE` | Full scrollback (same as `capture-pane -p -J -S -`) |
| `amux events` | Stream JSON events |
| `amux panes` | All panes (`list-panes -a`) |
| `amux plugin …` | Manage plugins (section 5) |
| `amux migrate [-S PATH \| -L NAME] [-t SESSION]… [--fresh \| --live] [--apply]` | Move tmux sessions into amux (section 6); without `--apply`, print the plan only |
| `agent-view [-t]`, `agent-next` | Dashboard / next agent needing attention (bound to `prefix A` / `prefix N`) |
| `plugin view [NAME]` | Plugin view (bound to `prefix P`) |
| `snapshot-panes` | Snapshot all pane scrollback now |
| `restart-server` | Re-exec supervisor and server onto the installed `amux` binary while keeping all panes running |
| `server-info` | Paths, counts, restart count and per-phase CPU timings |
| `panic-test [-k -t PANE]` | Crash the server (or one pane's emulator) to see recovery |

| Option | Default | |
|---|---|---|
| `agent-badge` | `on` | Agent badge in the status line, plus OSC 9 notifications |
| `agent-resume` | `typed` | After resurrection: `typed`, `run` or `off` for the agent's resume command |
| `plugins` | `on` | Run plugins from the plugin directory |
| `snapshot-interval` | `30` | Seconds between scrollback snapshots (0 disables) |
| `tmux-env` | `off` | Also set `$TMUX`/`$TMUX_PANE` in panes |

Files:
- Runtime (sockets): `$AMUX_RUNTIME_DIR/<name>`, else
  `$XDG_RUNTIME_DIR/amux/<name>`, else `/tmp/amux-$UID/<name>`.
- State: `$AMUX_STATE_DIR/<name>`, else `~/.local/state/amux/<name>`. It
  holds `state.json`, `panes/*.snap` and `amux.log`.
- `<name>` is `default` or the value of `-L`.
- Plugins: `$AMUX_PLUGIN_DIR`, else `$XDG_CONFIG_HOME/amux/plugins` (by
  default `~/.config/amux/plugins`). Their logs are in
  `<state>/plugins/NAME.log` and their data in `<state>/plugin-data/NAME/`.

## 8. Development

```sh
cargo test                                   # unit + e2e (real servers in temp dirs) + keeper tests
AMUX_ORACLE_CASES=200 cargo test --test oracle_tmux          # differential test vs tmux
AMUX_ORACLE_RESIZE=1 AMUX_ORACLE_MAXK=63 cargo test --test oracle_tmux
cargo build --release && python3 bench/bench.py              # benchmarks vs tmux
amux __bench-emu FILE                        # raw emulator throughput
AMUX_PROF=1 amux …                           # keepers log phase timings to amux.log
```

Source map:
- **Emulator:** `src/term.rs`, `src/history.rs`, `src/cell.rs`.
- **Pane process:** `src/keeper.rs`, `src/copymode.rs`.
- **Server:** `src/server/`, which covers commands, rendering, input and
  recovery.
- **Agents:** `src/agents.rs`.
- **tmux language:** `src/cmd.rs`, `src/format.rs`, `src/keys.rs`,
  `src/options.rs`.
- **Wire protocol:** `src/proto.rs`.
- **Plugins:** `src/server/plugins.rs` (the host), `sdk/` (Python SDK and
  templates), `docs/plugins.md` (the guide) and `examples/plugins/`.
- **CLI, attach client and supervisor:** `src/cli.rs`.
- **Migrating from tmux:**
  - `src/migrate.rs`: the plan;
  - `src/tmux.rs`: read-only tmux access;
  - `src/server/migrate.rs`: building the sessions;
  - `src/bridge.rs`: live panes.
