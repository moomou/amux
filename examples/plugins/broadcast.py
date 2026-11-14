#!/usr/bin/env python3
"""amux plugin: broadcast — send one message to many coding agents.

    amux broadcast [-a] [-f FILTER] MESSAGE...

Types MESSAGE and Enter into every agent that is waiting, done or idle
(-a: working ones too). -f keeps only agents whose name, directory, topic or
summary contains FILTER. Bind it if you like:

    bind B command-prompt -p "broadcast:" "broadcast '%%'"

Install: amux plugin install examples/plugins/broadcast.py
"""
import amux_plugin as amux


@amux.command("broadcast", help="broadcast [-a] [-f FILTER] MESSAGE: type MESSAGE into many agents")
def broadcast(ctx, *args):
    args = list(args)
    include_working, needle = False, ""
    while args and args[0].startswith("-"):
        flag = args.pop(0)
        if flag == "-a":
            include_working = True
        elif flag == "-f" and args:
            needle = args.pop(0).lower()
        elif flag == "--":
            break
        else:
            raise amux.AmuxError("unknown option %s (usage: broadcast [-a] [-f FILTER] MESSAGE)" % flag)
    text = " ".join(args)
    if not text:
        raise amux.AmuxError("usage: broadcast [-a] [-f FILTER] MESSAGE")
    states = {"waiting", "done", "idle"} | ({"working"} if include_working else set())
    sent = []
    for a in amux.agents():
        hay = " ".join(str(a.get(k) or "") for k in ("agent", "cwd", "topic", "summary")).lower()
        if a.pane == ctx.pane or a.state not in states or needle not in hay:
            continue
        amux.run("send-keys", "-t", a.pane, "-l", text)
        amux.run("send-keys", "-t", a.pane, "Enter")
        sent.append("%s (%s)" % (a.pane, a.agent))
    return "sent to %d agent(s): %s" % (len(sent), ", ".join(sent)) if sent else "no matching agents"


amux.main()
