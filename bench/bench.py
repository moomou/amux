#!/usr/bin/env python3
"""amux vs tmux: memory, output throughput and CPU use.

Runs each multiplexer in isolation (private socket/state dirs), one at a time.
Only the multiplexer's own processes are measured (tmux: server + client;
amux: supervisor + server + per-pane keepers + client), never the shells.

usage: bench/bench.py [--panes 10] [--lines 10000] [--mb 40] [--idle 20]
"""

import argparse
import fcntl
import os
import pty
import shutil
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AMUX = os.path.join(ROOT, "target", "release", "amux")
TMUX = shutil.which("tmux")
COLS, ROWS = 200, 50


def read_kv(path, key):
    try:
        with open(path) as f:
            for line in f:
                if line.startswith(key):
                    return int(line.split()[1])
    except (FileNotFoundError, ProcessLookupError, PermissionError):
        pass
    return 0


def pss_kb(pid):
    return read_kv(f"/proc/{pid}/smaps_rollup", "Pss:")


def rss_kb(pid):
    return read_kv(f"/proc/{pid}/status", "VmRSS:")


def cpu_ns(pid):
    """CPU time actually spent running (ns), from schedstat."""
    try:
        with open(f"/proc/{pid}/schedstat") as f:
            return int(f.read().split()[0])
    except (FileNotFoundError, ProcessLookupError):
        return 0


def cmdline(pid):
    try:
        with open(f"/proc/{pid}/cmdline", "rb") as f:
            return f.read().replace(b"\0", b" ").decode(errors="replace")
    except (FileNotFoundError, ProcessLookupError, PermissionError):
        return ""


def all_pids():
    return [int(p) for p in os.listdir("/proc") if p.isdigit()]


class Client:
    """An attached client on a private pty whose output is drained."""

    def __init__(self, argv, env):
        pid, fd = pty.fork()
        if pid == 0:
            os.execvpe(argv[0], argv, env)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        self.pid, self.fd = pid, fd
        self.alive = True
        threading.Thread(target=self._drain, daemon=True).start()

    def _drain(self):
        while self.alive:
            try:
                if not os.read(self.fd, 1 << 16):
                    break
            except OSError:
                break

    def close(self):
        self.alive = False
        try:
            os.kill(self.pid, 9)
            os.waitpid(self.pid, 0)
        except (ProcessLookupError, ChildProcessError):
            pass
        os.close(self.fd)


class Mux:
    name = ""

    def __init__(self, tmp):
        self.tmp = tmp
        self.client = None

    def run(self, *args, check=True):
        r = subprocess.run(self.argv(*args), env=self.env, capture_output=True, text=True, timeout=60)
        if check and r.returncode != 0:
            raise RuntimeError(f"{self.name} {args}: {r.stderr.strip()}")
        return r.stdout

    def attach(self, target):
        self.client = Client(self.argv("attach", "-t", target), self.env)
        time.sleep(1.0)

    def detach(self):
        if self.client:
            self.client.close()
            self.client = None
            time.sleep(0.3)

    def pids(self):
        v = self.server_pids()
        if self.client:
            v.append(self.client.pid)
        return v

    def pss(self):
        return sum(pss_kb(p) for p in self.pids())

    def rss(self):
        return sum(rss_kb(p) for p in self.pids())

    def cpu(self):
        return sum(cpu_ns(p) for p in self.pids())

    def cpu_by_pid(self):
        return {p: cpu_ns(p) for p in self.pids()}

    def send(self, target, text):
        self.run("send-keys", "-t", target, "-l", text)
        self.run("send-keys", "-t", target, "Enter")

    def wait_marker(self, target, marker, timeout=120):
        t0 = time.time()
        while time.time() - t0 < timeout:
            if marker in self.run("capture-pane", "-p", "-t", target):
                return
            time.sleep(0.25)
        raise RuntimeError(f"{self.name}: timed out waiting for {marker} in {target}")


class Tmux(Mux):
    name = "tmux"

    def __init__(self, tmp):
        super().__init__(tmp)
        self.conf = os.path.join(tmp, "tmux.conf")
        with open(self.conf, "w") as f:
            f.write("set -g history-limit 10000\nset -g default-terminal xterm-256color\n")
        self.env = dict(os.environ, TMUX_TMPDIR=tmp, SHELL="/bin/bash")
        self.env.pop("TMUX", None)

    def argv(self, *args):
        return [TMUX, "-L", "bench", "-f", self.conf, *args]

    def server_pids(self):
        try:
            return [int(self.run("display", "-p", "#{pid}").strip())]
        except Exception:
            return []

    def kill(self):
        self.run("kill-server", check=False)


class Amux(Mux):
    name = "amux"

    def __init__(self, tmp):
        super().__init__(tmp)
        self.conf = os.path.join(tmp, "amux.conf")
        with open(self.conf, "w") as f:
            f.write("set -g history-limit 10000\nset -g default-terminal xterm-256color\n")
        self.env = dict(
            os.environ,
            AMUX_RUNTIME_DIR=os.path.join(tmp, "run"),
            AMUX_STATE_DIR=os.path.join(tmp, "state"),
            # An empty plugin directory: measure amux itself, not your plugins.
            AMUX_PLUGIN_DIR=os.path.join(tmp, "plugins"),
            SHELL="/bin/bash",
        )
        for k in ("TMUX", "AMUX_PANE", "AMUX"):
            self.env.pop(k, None)

    def argv(self, *args):
        return [AMUX, "-L", "bench", "-f", self.conf, *args]

    def server_pids(self):
        rt = self.env["AMUX_RUNTIME_DIR"]
        out = []
        for p in all_pids():
            c = cmdline(p)
            if "__server bench" in c or "__supervisor bench" in c or ("__keeper" in c and rt in c):
                out.append(p)
        return out

    def kill(self):
        self.run("kill-server", check=False)


def fill_cmd(lines):
    return (
        "awk 'BEGIN{for(i=1;i<=%d;i++) printf \"%%06d lorem ipsum dolor sit amet, consectetur adipiscing elit, "
        "sed do eiusmod tempor %%d\\n\", i, i*7}'; echo FILL\"\"DONE" % lines
    )


def bench(mux, args, big):
    r = {}
    targets = []
    mux.run("new-session", "-d", "-s", "b", "-x", str(COLS), "-y", str(ROWS))
    targets.append("b:0")
    for i in range(1, args.panes):
        mux.run("new-window", "-d", "-t", "b:")
        targets.append(f"b:{i}")
    time.sleep(1.5)
    r["idle_pss_kb"] = mux.pss()
    r["idle_rss_kb"] = mux.rss()
    r["procs"] = len(mux.server_pids())

    # Scrollback: N lines of ~85 chars in every pane.
    for t in targets:
        mux.send(t, fill_cmd(args.lines))
    for t in targets:
        mux.wait_marker(t, "FILLDONE")
    time.sleep(2.5)
    r["hist_pss_kb"] = mux.pss()
    r["hist_rss_kb"] = mux.rss()

    # Idle CPU with a client attached (status line clock running).
    mux.attach("b:0")
    c0 = mux.cpu()
    time.sleep(args.idle)
    r["idle_cpu_ms"] = (mux.cpu() - c0) / 1e6

    # An agent-like spinner (10 Hz redraw of one line) in the visible pane.
    mux.send("b:0", "clear")
    time.sleep(0.5)
    # One long-running process redrawing a status line, like a coding agent.
    spin = (
        "python3 -c 'import sys,time,random; w=sys.stdout.write; f=sys.stdout.flush; "
        "[(w(\"\\r\\033[2K\\u280b Thinking\\u2026 (%%ds \\u00b7 esc to interrupt) %%d\" %% (i//10, random.randint(0,99999))), f(), time.sleep(0.1)) for i in range(%d)]'"
        % (args.idle * 10 + 20)
    )
    mux.send("b:0", spin)
    time.sleep(0.5)
    c0 = mux.cpu()
    b0 = mux.cpu_by_pid()
    time.sleep(args.idle)
    r["spinner_cpu_ms"] = (mux.cpu() - c0) / 1e6
    if os.environ.get("BENCH_DEBUG"):
        b1 = mux.cpu_by_pid()
        for p in sorted(b1, key=lambda p: b0.get(p, 0) - b1[p]):
            d = (b1[p] - b0.get(p, b1[p])) / 1e6
            if d > 0.2:
                print(f"  {mux.name} pid {p} {cmdline(p)[:40]!r}: {d:.1f} ms", file=sys.stderr)
    time.sleep(1.0)

    # Throughput: cat a big file in the visible pane.
    stamp = os.path.join(mux.tmp, "tput_us")
    mux.send("b:0", "clear")
    time.sleep(0.5)
    c0 = mux.cpu()
    mux.send("b:0", f"s=$(date +%s%N); cat {big}; e=$(date +%s%N); echo $(( (e-s)/1000 )) > {stamp}")
    t0 = time.time()
    while not os.path.exists(stamp) or os.path.getsize(stamp) == 0:
        if time.time() - t0 > 300:
            raise RuntimeError("throughput run timed out")
        time.sleep(0.05)
    us = int(open(stamp).read().strip())
    time.sleep(1.0)  # let the multiplexer finish rendering
    r["cat_s"] = us / 1e6
    r["cat_mb_s"] = os.path.getsize(big) / 1e6 / (us / 1e6)
    r["cat_cpu_ms"] = (mux.cpu() - c0) / 1e6
    r["peak_pss_kb"] = mux.pss()
    mux.detach()
    return r


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--panes", type=int, default=10)
    ap.add_argument("--lines", type=int, default=10000)
    ap.add_argument("--mb", type=int, default=40)
    ap.add_argument("--idle", type=int, default=20)
    ap.add_argument("--only", choices=["tmux", "amux"])
    ap.add_argument("--reps", type=int, default=3, help="alternating repetitions; medians are reported")
    args = ap.parse_args()
    if not os.path.exists(AMUX):
        sys.exit("build first: cargo build --release")
    tmp_root = tempfile.mkdtemp(prefix="amux-bench-")
    big = os.path.join(tmp_root, "big.txt")
    with open(big, "w") as f:
        line = "%08d the quick brown fox jumps over the lazy dog \x1b[32mok\x1b[0m 0123456789 abcdefghij\n"
        n = 0
        while f.tell() < args.mb * 1_000_000:
            f.write(line % n)
            n += 1
    runs = {}
    try:
        for rep in range(args.reps):
            for cls in (Tmux, Amux):
                if args.only and cls.name != args.only:
                    continue
                tmp = os.path.join(tmp_root, f"{cls.name}{rep}")
                os.makedirs(tmp)
                mux = cls(tmp)
                try:
                    print(f"run {rep + 1}/{args.reps}: {cls.name}...", file=sys.stderr)
                    runs.setdefault(cls.name, []).append(bench(mux, args, big))
                finally:
                    mux.detach()
                    mux.kill()
                    time.sleep(0.5)
    finally:
        shutil.rmtree(tmp_root, ignore_errors=True)
    # Median of each metric across repetitions (the host may be busy).
    results = {n: {k: sorted(r[k] for r in rs)[len(rs) // 2] for k in rs[0]} for n, rs in runs.items()}

    rows = [
        ("processes (server side)", "procs", "{:.0f}"),
        (f"memory, {args.panes} idle shells (PSS MB)", "idle_pss_kb", "{:.1f}", 1 / 1024),
        (f"memory, + {args.lines} lines scrollback each (PSS MB)", "hist_pss_kb", "{:.1f}", 1 / 1024),
        ("memory after big cat (PSS MB)", "peak_pss_kb", "{:.1f}", 1 / 1024),
        (f"idle CPU over {args.idle}s, attached (ms)", "idle_cpu_ms", "{:.1f}"),
        (f"10 Hz spinner CPU over {args.idle}s (ms)", "spinner_cpu_ms", "{:.1f}"),
        (f"cat {args.mb} MB: wall time (s)", "cat_s", "{:.2f}"),
        (f"cat {args.mb} MB: throughput (MB/s)", "cat_mb_s", "{:.1f}"),
        (f"cat {args.mb} MB: multiplexer CPU (ms)", "cat_cpu_ms", "{:.0f}"),
    ]
    names = list(results)
    print("| metric | " + " | ".join(names) + " |")
    print("|---|" + "---|" * len(names))
    for row in rows:
        label, key, fmt = row[0], row[1], row[2]
        scale = row[3] if len(row) > 3 else 1
        print(f"| {label} | " + " | ".join(fmt.format(results[n][key] * scale) for n in names) + " |")


if __name__ == "__main__":
    main()
