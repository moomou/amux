#!/usr/bin/env python3
"""amux plugin: {{NAME}}

Created by `amux plugin new`. Saving this file reloads it immediately.
Try it:  amux {{NAME}} world
Debug:   amux plugin show {{NAME}}    amux plugin logs {{NAME}}
Guide:   amux plugin guide
"""
import amux_plugin as amux


@amux.command("{{NAME}}", help="Example command: say hello")
def {{IDENT}}(ctx, *args):
    # ctx has the caller's pane, window, session and cwd. Return text to show.
    who = " ".join(args) or "world"
    return f"hello, {who}! (from pane {ctx.pane})"


@amux.on("agent-state")
def on_agent_state(ev):
    # Runs whenever a coding agent changes state: working, waiting, done, idle.
    if ev.state == "waiting":
        amux.log(f"{ev.agent} in {ev.pane} is waiting: {ev.summary}")


amux.main()
