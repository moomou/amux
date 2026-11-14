#!/usr/bin/env python3
"""amux plugin: worktree — one git worktree per agent.

    amux worktree NAME [COMMAND...]

Run from a pane inside a git repository. Creates branch NAME in a new
worktree next to the repository (../REPO-NAME), opens a window there and
runs COMMAND in it, e.g. `amux worktree fix-login claude`. Parallel agents
then never edit the same checkout. Agents working in a linked worktree get a
‹⎇ BRANCH +ADDED -REMOVED› tag in the agent dashboard.

Install: amux plugin install examples/plugins/worktree.py
"""
import os
import re
import subprocess

import amux_plugin as amux


def git(cwd, *args):
    return subprocess.run(["git", "-C", cwd, *args], capture_output=True, text=True, timeout=30)


@amux.command("worktree", help="worktree NAME [COMMAND...]: new git worktree and window for an agent")
def worktree(ctx, name=None, *command):
    if not name:
        raise amux.AmuxError("usage: worktree NAME [COMMAND...]")
    top = git(ctx.cwd or os.getcwd(), "rev-parse", "--show-toplevel")
    if top.returncode != 0:
        raise amux.AmuxError("not inside a git repository: %s" % ctx.cwd)
    root = top.stdout.strip()
    path = os.path.join(os.path.dirname(root), "%s-%s" % (os.path.basename(root), name))
    made = git(root, "worktree", "add", "-b", name, path)
    if made.returncode != 0:
        made = git(root, "worktree", "add", path, name)  # the branch may exist already
    if made.returncode != 0:
        raise amux.AmuxError(made.stderr.strip() or "git worktree add failed")
    argv = ["new-window", "-n", name, "-c", path]
    if command:
        argv.append(" ".join(command))
    amux.run(*argv)
    return "worktree %s on branch %s" % (path, name)


@amux.every(20)
def tag_agents():
    for a in amux.agents():
        if not a.cwd:
            continue
        gitdir = git(a.cwd, "rev-parse", "--absolute-git-dir").stdout.strip()
        common = git(a.cwd, "rev-parse", "--git-common-dir").stdout.strip()
        if not gitdir or not common or os.path.realpath(gitdir) == os.path.realpath(os.path.join(a.cwd, common)):
            continue  # not in a linked worktree
        branch = git(a.cwd, "rev-parse", "--abbrev-ref", "HEAD").stdout.strip()
        stat = git(a.cwd, "diff", "--shortstat", "HEAD").stdout
        added = re.search(r"(\d+) insertion", stat)
        removed = re.search(r"(\d+) deletion", stat)
        amux.annotate(a.pane, "⎇ %s +%s -%s" % (branch, added.group(1) if added else 0, removed.group(1) if removed else 0))


amux.main()
