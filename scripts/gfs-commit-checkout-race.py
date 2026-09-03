#!/usr/bin/env python3
"""Try to make a concurrent commit and checkout lose a commit.

Usage: gfs-commit-checkout-race.py <repo> <gfs-binary> <rounds> [delay-seconds]

`<repo>` must be an initialised GFS repo with two branches, `main` and `other`,
each with at least one commit. `delay-seconds` (default 0.05) is how long after
the commit starts the checkout is launched.

WHAT IT LOOKS FOR. A commit reads HEAD's commit as its new commit's PARENT
before taking the snapshot, and reads HEAD's BRANCH after, when it advances the
ref. Checkout writes HEAD in between. The result is a commit parented on the
branch checkout moved away FROM, written onto the branch it moved TO — so that
branch's previous tip stops being reachable from any ref. Both commands report
success.

Three checks run each round:

  lost         a branch tip no longer reaches a commit it reached before;
  mismatch     a commit's snapshot holds the other branch's data, read from a
               `marker(branch TEXT)` table when the workspace has one;
  inconsistent HEAD names one branch while .gfs/WORKSPACE points into another.

MAKING IT REPRODUCE. The window is the snapshot copy, so the workspace has to
be big enough for that to take real time — a few thousand files is enough; with
a 0.5-second commit this reproduced four times out of four.

A workspace holding a live SQLite database is partly protected by accident:
checkout quiesces the database first, and that `BEGIN IMMEDIATE` blocks on the
write lock the commit's snapshot guard holds (measured at 550 ms against 34 ms
unobstructed). That protection is not a fix — it does not exist before the
first write, does not cover container-backed providers, and does not cover the
window before the guard is taken — so to see the race, use a workspace with
files but no database.

Exits 0 when nothing went wrong, and reports how many rounds actually ran the
two commands concurrently to success: a run where they never overlapped proves
nothing.
"""
import json
import os
import sqlite3
import subprocess
import sys
import threading
import time

if len(sys.argv) < 4:
    sys.exit(__doc__)
repo, gfs, rounds = sys.argv[1], sys.argv[2], int(sys.argv[3])
DELAY = float(sys.argv[4]) if len(sys.argv) > 4 else 0.05

OBJ = os.path.join(repo, ".gfs", "objects")
HEADS = os.path.join(repo, ".gfs", "refs", "heads")


def run(*args):
    return subprocess.run([gfs, *args], cwd=repo, capture_output=True, text=True)


def load(commit_hash):
    try:
        with open(os.path.join(OBJ, commit_hash[:2], commit_hash[2:])) as fh:
            return json.load(fh)
    except (OSError, ValueError):
        return None


def commits():
    found = {}
    for prefix in os.listdir(OBJ):
        directory = os.path.join(OBJ, prefix)
        if not os.path.isdir(directory):
            continue
        for rest in os.listdir(directory):
            obj = load(prefix + rest)
            if obj and "snapshot_hash" in obj:
                found[prefix + rest] = obj
    return found


def reachable(tip):
    seen, stack = set(), [tip]
    while stack:
        current = stack.pop()
        if current in seen:
            continue
        seen.add(current)
        obj = load(current)
        if obj:
            stack.extend(obj.get("parents") or [])
    return seen


def marker(directory):
    """Which branch's data is here, per the optional `marker` table."""
    db = os.path.join(directory, "db.sqlite")
    if not os.path.exists(db):
        return None
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    try:
        return conn.execute("SELECT branch FROM marker LIMIT 1").fetchone()[0]
    except Exception:
        return None
    finally:
        conn.close()


def snapshot_dir(obj):
    h = obj["snapshot_hash"]
    return os.path.join(repo, ".gfs", "snapshots", h[:2], h[2:])


def head_name():
    head = open(os.path.join(repo, ".gfs", "HEAD")).read().strip()
    return head.rsplit("/", 1)[-1] if head.startswith("ref:") else "detached"


def workspace_branch():
    parts = open(os.path.join(repo, ".gfs", "WORKSPACE")).read().strip().split(os.sep)
    return parts[parts.index("workspaces") + 1] if "workspaces" in parts else None


def tips():
    return {n: open(os.path.join(HEADS, n)).read().strip() for n in os.listdir(HEADS)}


known = set(commits())
before = {name: reachable(tip) for name, tip in tips().items()}
lost, mismatched, inconsistent, concurrent = [], [], [], 0

for i in range(rounds):
    source, target = ("main", "other") if i % 2 == 0 else ("other", "main")
    run("checkout", source)
    results = {}

    def do_commit():
        results["commit"] = run("commit", "-m", f"race{i}")

    def do_checkout():
        time.sleep(DELAY)
        results["checkout"] = run("checkout", target)

    threads = [threading.Thread(target=do_commit), threading.Thread(target=do_checkout)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    if results["commit"].returncode == 0 and results["checkout"].returncode == 0:
        concurrent += 1

    head, workspace = head_name(), workspace_branch()
    if head != "detached" and workspace and head != workspace:
        inconsistent.append(i)
        print(f"  INCONSISTENT round {i}: HEAD is '{head}' but WORKSPACE is in '{workspace}'")

    now = {name: reachable(tip) for name, tip in tips().items()}
    for name, was in before.items():
        dropped = was - now.get(name, set())
        if dropped:
            lost.append((i, name, sorted(dropped)[0]))
            print(f"  LOST round {i}: branch '{name}' no longer reaches {sorted(dropped)[0][:8]}")
    before = now

    for commit_hash, obj in commits().items():
        if commit_hash in known:
            continue
        known.add(commit_hash)
        owners = [n for n, reach in now.items() if commit_hash in reach]
        if len(owners) != 1:
            continue
        held = marker(snapshot_dir(obj))
        if held and held != owners[0]:
            mismatched.append((commit_hash, owners[0], held))
            print(f"  MISMATCH commit {commit_hash[:8]} on '{owners[0]}' holds '{held}' data")

print(f"  {concurrent}/{rounds} rounds ran commit and checkout concurrently to success")
print(f"  RESULT: {len(lost)} lost, {len(mismatched)} mismatched, {len(inconsistent)} inconsistent")
if lost or mismatched or inconsistent:
    sys.exit(1)
if concurrent == 0:
    print("  INCONCLUSIVE: the two commands never overlapped")
    sys.exit(2)
