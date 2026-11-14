#!/usr/bin/env python3
"""amux plugin: stuck — flag agents that look stuck.

An agent that stays `working` while its screen shows no real progress for
@stuck-minutes (default 5) gets a ‹stuck? Nm› tag in the agent dashboard and
a desktop notification. Spinners and elapsed-time counters do not count as
progress. Configure with: set -g @stuck-minutes 10

Install: amux plugin install examples/plugins/stuck.py
"""
import re
import time

import amux_plugin as amux

# Spinner glyphs, counters and punctuation that change while nothing happens.
_NOISE = re.compile(r"[\d\u2800-\u28ff✻✽✶✢✳·•●○◐◓◑◒|/\\\-().:]+")
_seen = {}  # pane -> (normalized screen tail, when it last changed)
_flagged = set()


def _limit_seconds():
    try:
        return float(amux.run("show-options -gqv @stuck-minutes").strip() or 5) * 60
    except (amux.AmuxError, ValueError):
        return 300.0


def _clear(pane):
    if pane in _flagged:
        _flagged.discard(pane)
        amux.annotate(pane, "")


@amux.on("output")
def output(ev):
    key = tuple(_NOISE.sub("", line).strip() for line in ev.lines or [])
    prev = _seen.get(ev.pane)
    if prev is None or prev[0] != key:
        _seen[ev.pane] = (key, time.monotonic())
        _clear(ev.pane)


@amux.on("agent-state")
def state(ev):
    if ev.state == "working":
        _seen.setdefault(ev.pane, ((), time.monotonic()))
    else:
        _clear(ev.pane)


@amux.on("pane-closed")
def closed(ev):
    _seen.pop(ev.pane, None)
    _flagged.discard(ev.pane)


@amux.every(2)
def check():
    limit, now = _limit_seconds(), time.monotonic()
    for a in amux.agents():
        if a.state != "working" or a.pane in _flagged:
            continue
        since = _seen.setdefault(a.pane, ((), now))[1]
        if now - since >= limit:
            _flagged.add(a.pane)
            idle = now - since
            took = "%dm" % (idle // 60) if idle >= 60 else "%ds" % idle
            amux.annotate(a.pane, "stuck? " + took)
            amux.notify("%s in %s:%s shows no progress for %s" % (a.agent, a.session, a.window, took), pane=a.pane)


amux.main()
