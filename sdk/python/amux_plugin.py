"""amux plugin SDK for Python (plugin API 1).

amux puts this module on PYTHONPATH for every plugin it starts. A plugin
declares what it adds, then calls main():

    import amux_plugin as amux

    @amux.command("hello", help="Say hello")
    def hello(ctx, *args):
        return "hello " + " ".join(args)

    @amux.on("agent-state")
    def changed(ev):
        if ev.state == "waiting":
            amux.notify(f"{ev.agent} needs you: {ev.summary}")

    amux.main()

amux undoes everything a plugin registered when it stops, reloads or crashes.
stdout carries the protocol, so print() output is redirected to the plugin log
(`amux plugin logs NAME`). Full guide: `amux plugin guide`.
"""

import json
import os
import select
import sys
import time
import traceback

API = 1
name = os.environ.get("AMUX_PLUGIN") or os.path.splitext(os.path.basename(sys.argv[0] or "plugin"))[0]
data_dir = os.environ.get("AMUX_PLUGIN_DATA") or os.path.join(os.path.expanduser("~"), ".cache", "amux-plugin-" + name)
info = {}  # amux's hello message: api, amux version, server name, directories

# The real stdout is the protocol channel; stray print() calls go to the log.
_proto = sys.stdout
sys.stdout = sys.stderr


class AmuxError(Exception):
    """A command run through amux failed, or amux rejected a request."""


class Obj(dict):
    """A dict whose keys also read as attributes; missing keys read as None."""

    def __getattr__(self, key):
        return self.get(key)


_commands = {}
_handlers = {}
_keys = []
_agents = []
_timers = []
_serving = False
_next_id = 0
_replies = {}
_buf = b""
_ctx = None


# ----- declaring what the plugin adds ---------------------------------------


def command(name=None, help="", usage=""):
    """Decorator: add a command. The handler gets (ctx, *args) and returns
    text (str), lines (list), a dict, or None."""

    def deco(fn):
        cname = name or fn.__name__.replace("_", "-")
        doc = (fn.__doc__ or "").strip().splitlines()
        _commands[cname] = {"fn": fn, "help": help or (doc[0] if doc else ""), "usage": usage}
        _reregister()
        return fn

    return deco


def on(*events):
    """Decorator: handle events, e.g. @on("agent-state"). "*" means every
    event except "output"."""

    def deco(fn):
        for e in events or (fn.__name__.replace("_", "-"),):
            _handlers.setdefault(e, []).append(fn)
        _reregister()
        return fn

    return deco


def every(seconds):
    """Decorator: call the function every `seconds` (first call after one period)."""

    def deco(fn):
        _timers.append([float(seconds), time.monotonic() + float(seconds), fn])
        return fn

    return deco


def key(key, command, table="prefix", note=""):
    """Bind a key to a command string. The previous binding comes back when
    the plugin unloads."""
    _keys.append({"key": key, "command": command, "table": table, "note": note})
    _reregister()


def agent(name, cmd, working=(), waiting=(), chrome=(), resume=None):
    """Teach amux to recognise a coding agent CLI, or replace the built-in
    definition of the same name (`amux agents --kinds` lists them).

    cmd is a regex matched against the command line (the program and its
    first argument). working, waiting and chrome are case-insensitive regexes
    for screen lines meaning "working", "needs the user" and "UI chrome, never
    a summary"; patterns shared by agent TUIs are always added. resume is
    typed into a shell to continue the last conversation after a reboot."""
    entry = {"name": name, "cmd": cmd, "working": list(working), "waiting": list(waiting), "chrome": list(chrome)}
    if resume:
        entry["resume"] = resume
    _agents[:] = [a for a in _agents if a["name"] != name] + [entry]
    _reregister()


# ----- talking to amux ------------------------------------------------------


def run(*args, pane=None, check=True):
    """Run a tmux/amux command; return its stdout.

        run("display-message -p '#{session_name}'")   # one string: tmux syntax
        run("send-keys", "-t", "%3", "y", "Enter")      # several: an argv

    Inside a command handler the caller's pane is the default target.
    Raises AmuxError if the command fails (unless check=False).
    """
    if not args:
        raise ValueError("run() needs a command")
    params = {"command": args[0]} if len(args) == 1 else {"argv": [str(a) for a in args]}
    target = pane or (_ctx.pane if _ctx is not None else None)
    if target:
        params["pane"] = target
    res = _call("run", params)
    code = res.get("code", 0)
    if check and code != 0:
        raise AmuxError((res.get("stderr") or "").strip() or "exit status %d" % code)
    return res.get("stdout", "")


def run_json(*args, **kw):
    """run() and parse the output as JSON (objects allow attribute access)."""
    return json.loads(run(*args, **kw) or "null", object_hook=Obj)


def agents():
    """Coding agents amux knows about (see `amux agents --json`)."""
    return run_json("agents", "--json") or []


def status(text):
    """Show text in the status line (tmux #[style] markup allowed); "" clears."""
    _notify("status", {"text": str(text)})


def annotate(pane, text):
    """Tag a pane's agent in the dashboard and `amux agents`; "" clears."""
    _notify("annotate", {"pane": pane, "text": str(text)})


def notify(message, pane=None):
    """Desktop notification plus a message on attached clients."""
    params = {"message": str(message)}
    if pane:
        params["pane"] = pane
    _notify("notify", params)


def log(*parts):
    """Write a line to the plugin log."""
    print(*parts, file=sys.stderr, flush=True)


def main():
    """Register and serve amux requests until amux closes stdin."""
    global _serving
    _serving = True
    _register()
    while True:
        _drain()
        _run_timers()
        timeout = None
        if _timers:
            timeout = max(0.0, min(t[1] for t in _timers) - time.monotonic())
        try:
            ready, _, _ = select.select([0], [], [], timeout)
        except InterruptedError:
            continue
        if ready and not _read_some():
            return  # amux closed stdin: stop quietly


# ----- internals --------------------------------------------------------------


def _send(obj):
    _proto.write(json.dumps(obj, separators=(",", ":")) + "\n")
    _proto.flush()


def _notify(method, params):
    _send({"jsonrpc": "2.0", "method": method, "params": params})


def _call(method, params):
    global _next_id
    _next_id += 1
    rid = _next_id
    _send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
    # Serve amux's own requests while waiting (a command may call run()).
    while rid not in _replies:
        msg = _next_message()
        if msg is None:
            raise AmuxError("amux closed the connection")
        _dispatch(msg)
    reply = _replies.pop(rid)
    if reply.get("error") is not None:
        err = reply["error"]
        raise AmuxError(err.get("message", str(err)) if isinstance(err, dict) else str(err))
    return reply.get("result") or {}


def _read_some():
    global _buf
    try:
        chunk = os.read(0, 65536)
    except InterruptedError:
        return True
    if not chunk:
        return False
    _buf += chunk
    return True


def _pop_line():
    global _buf
    i = _buf.find(b"\n")
    if i < 0:
        return None
    line, _buf = _buf[:i], _buf[i + 1 :]
    return line


def _parse(line):
    if not line.strip():
        return None
    try:
        return json.loads(line)
    except ValueError:
        log("amux_plugin: ignoring malformed message:", line[:200])
        return None


def _next_message():
    while True:
        line = _pop_line()
        if line is None:
            if not _read_some():
                return None
            continue
        msg = _parse(line)
        if msg is not None:
            return msg


def _drain():
    while True:
        line = _pop_line()
        if line is None:
            return
        msg = _parse(line)
        if msg is not None:
            _dispatch(msg)


def _dispatch(msg):
    method = msg.get("method")
    if method is None:
        if "id" in msg:
            _replies[msg["id"]] = msg
        return
    params = Obj(msg.get("params") or {})
    rid = msg.get("id")
    if method == "command":
        _handle_command(rid, params)
    elif method == "event":
        _handle_event(params)
    elif method == "hello":
        info.update(params)
    elif method == "shutdown":
        sys.exit(0)
    elif rid is not None:
        _send({"jsonrpc": "2.0", "id": rid, "error": {"code": -32601, "message": "unknown method: " + method}})


def _handle_command(rid, params):
    global _ctx
    entry = _commands.get(params.name)
    if entry is None:
        reply = {"error": {"code": 1, "message": "unknown command: %s" % params.name}}
    else:
        prev, _ctx = _ctx, params
        try:
            reply = {"result": _result(entry["fn"](params, *(params.args or [])))}
        except AmuxError as e:
            reply = {"error": {"code": 1, "message": str(e)}}
        except Exception as e:  # report to the caller, keep serving
            traceback.print_exc()
            reply = {"error": {"code": 1, "message": "%s: %s" % (type(e).__name__, e)}}
        finally:
            _ctx = prev
    if rid is not None:
        reply.update({"jsonrpc": "2.0", "id": rid})
        _send(reply)


def _result(value):
    if value is None:
        return ""
    if isinstance(value, (list, tuple)):
        return [str(v) for v in value]
    if isinstance(value, dict):
        return value
    return str(value)


def _handle_event(ev):
    fns = list(_handlers.get(ev.event, []))
    if ev.event != "output":
        fns += _handlers.get("*", [])
    for fn in fns:
        try:
            fn(ev)
        except Exception:
            traceback.print_exc()


def _run_timers():
    now = time.monotonic()
    for t in _timers:
        if now >= t[1]:
            t[1] = now + t[0]
            try:
                t[2]()
            except Exception:
                traceback.print_exc()


def _register():
    params = {
        "commands": [{"name": n, "help": c["help"], "usage": c["usage"]} for n, c in _commands.items()],
        "events": sorted(_handlers),
        "keys": list(_keys),
        "agents": list(_agents),
    }
    for r in _call("register", params).get("rejected") or []:
        log("amux_plugin: amux rejected %s: %s" % (r.get("name"), r.get("reason")))


def _reregister():
    if _serving:
        _register()
