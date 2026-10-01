#!/usr/bin/env python3
"""Try to make a concurrent commit and checkout lose a commit.

Usage: gfs-commit-checkout-race.py <repo> <gfs-binary> <rounds> [delay-seconds]

`<repo>` must be an initialised GFS repo with two branches, `main` and `other`,
each with at least one commit.

`delay-seconds` (default 0.05) is SIGNED: positive launches the checkout that
long after the commit, negative launches the COMMIT that long after the
checkout. The sign matters and the right one changes with the tree. Once
checkout began always restoring from the snapshot it grew from ~0.03s to ~0.49s
on a 2500-file workspace, overtaking the ~0.43s commit -- and since checkout
writes HEAD at the END of its work, no positive delay can put that write inside
the commit's window any more. Sweep both signs before concluding anything.

WHAT IT LOOKS FOR. A commit reads HEAD's commit as its new commit's PARENT
before taking the snapshot, and reads HEAD's BRANCH after, when it advances the
ref. Checkout writes HEAD in between. The result is a commit parented on the
branch checkout moved away FROM, written onto the branch it moved TO — so that
branch's previous tip stops being reachable from any ref. Both commands report
success.

Four checks run each round:

  misparented  a commit written onto branch B does not have B's previous tip as
               a parent. This is the primary signal: it is an invariant no
               legitimate commit can break, it fires every round the race fires,
               and it needs no history to compare against;
  lost         a branch tip no longer reaches a commit it reached before.
               SATURATES -- it reports at most once per run, because after the
               first loss the two branches are linearly entangled and no later
               interleave makes anything unreachable. Where `misparented` fired
               20/20, `lost` fired once. Do not read it as an incidence rate;
  mismatch     a commit's snapshot holds the other branch's data, read from a
               `marker(branch TEXT)` table when the workspace has one;
  inconsistent HEAD names one branch while .gfs/WORKSPACE points into another.

MAKING IT REPRODUCE. Both the workspace size and the delay's sign have to suit
the tree under test, and the combination that works has already changed once.
Measured figures, database-less workspace, APFS:

  500 files,  delay  0.0    reproduces on current main
  2500 files, delay -0.1    reproduces on current main, 6/6 rounds
  2500 files, delay +0.05   reproduces only on trees where checkout is cheaper
                            than commit; on current main it reaches nothing

Sweep size and both signs. A single clean configuration is not a result.

A workspace holding a live database may be partly protected by accident if the
provider quiesces on checkout and that blocks against the snapshot guard. No
such provider is in this tree -- the registered ones are postgresql, mysql and
clickhouse, all container-backed -- so this is a caveat for elsewhere, not a
description of what runs here. Use files and no database either way: it is the
configuration the window is widest in.

CALIBRATE IT BEFORE YOU TRUST A GREEN. Point it at a build with the lock
defeated -- stub `file.try_lock()` to `Ok(())` in repo_utils/repo_lock.rs -- and
confirm it goes red. Calibrating against an older ref instead is not enough:
an older ref differs in more than the fix, and at one point this script passed
at its own documented settings against a binary with the lock removed, because
the window had moved outside the delays it could express.

Overlap is measured from process start and end times, not inferred from exit
codes. A refused command still overlapped; a command that ran alone did not.
Exits 0 when nothing went wrong, 1 on a finding, 2 when no round overlapped.
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


def probe_window():
    """Time an unobstructed commit and checkout, and derive the usable delays.

    The race needs checkout's HEAD write to land inside the commit's window.
    Checkout writes HEAD last, so that write happens at about `delay + Tk`,
    and the commit advances its ref at about `Tc`. The window is therefore
    reachable only while `delay < Tc - Tk`.

    This is measured rather than assumed because the sign of `Tc - Tk` has
    already flipped once: when checkout began always restoring from the
    snapshot it went from far cheaper than commit to slightly dearer, and
    every positive delay stopped reaching. A run outside the usable range
    cannot fail, so reporting it as a pass is worse than not running it.
    """
    names = sorted(tips())
    here, there = names[0], names[1 % len(names)]
    run("checkout", here)
    workspace = open(os.path.join(repo, ".gfs", "WORKSPACE")).read().strip()
    with open(os.path.join(workspace, ".race-probe"), "w") as fh:
        fh.write("probe")
    t0 = time.monotonic()
    run("commit", "-m", "window probe")
    tc = time.monotonic() - t0
    t0 = time.monotonic()
    run("checkout", there)
    tk = time.monotonic() - t0
    run("checkout", here)
    return tc, tk


TC, TK = probe_window()
USABLE_BELOW = TC - TK
REACHABLE = DELAY < USABLE_BELOW
print(f"  probe: commit {TC:.3f}s, checkout {TK:.3f}s -> delays below "
      f"{USABLE_BELOW:+.3f}s can reach the window; this run uses {DELAY:+.3f}s")
if not REACHABLE:
    print(f"  WARNING: {DELAY:+.3f}s cannot place checkout's HEAD write inside the "
          f"commit's window on this build. This run cannot fail, whatever the code does.")

known = set(commits())
before = {name: reachable(tip) for name, tip in tips().items()}
lost, mismatched, inconsistent, misparented = [], [], [], []
overlapped = 0   # rounds where the two processes were alive at the same instant
both_ok = 0      # rounds where both also exited 0

for i in range(rounds):
    source, target = ("main", "other") if i % 2 == 0 else ("other", "main")
    run("checkout", source)
    results, spans = {}, {}
    # Tips as they stood before this round, so a new commit can be checked
    # against the tip its own branch actually had.
    tips_before = tips()

    # A negative delay makes the CHECKOUT lead. Needed once checkout outlasts
    # commit: checkout writes HEAD last, so with a positive delay that write
    # lands after the commit's ref advance and never enters the window.
    def do_commit():
        if DELAY < 0:
            time.sleep(-DELAY)
        t0 = time.monotonic()
        results["commit"] = run("commit", "-m", f"race{i}")
        spans["commit"] = (t0, time.monotonic())

    def do_checkout():
        if DELAY > 0:
            time.sleep(DELAY)
        t0 = time.monotonic()
        results["checkout"] = run("checkout", target)
        spans["checkout"] = (t0, time.monotonic())

    threads = [threading.Thread(target=do_commit), threading.Thread(target=do_checkout)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    # Overlap is a fact about wall time, not about exit codes: a command that
    # was REFUSED still overlapped, and two that never met can both exit 0.
    (c0, c1), (k0, k1) = spans["commit"], spans["checkout"]
    if min(c1, k1) > max(c0, k0):
        overlapped += 1
    if results["commit"].returncode == 0 and results["checkout"].returncode == 0:
        both_ok += 1

    head, workspace = head_name(), workspace_branch()
    if head != "detached" and workspace and head != workspace:
        inconsistent.append(i)
        print(f"  INCONSISTENT round {i}: HEAD is '{head}' but WORKSPACE is in '{workspace}'")

    # THE PRIMARY SIGNAL. A commit written onto branch B must have B's previous
    # tip as a parent. Nothing legitimate breaks that: a commit advances the
    # branch it is on, from where that branch already was. When checkout moves
    # HEAD between the commit reading its parent and the commit advancing a
    # ref, the new tip carries the OTHER branch's commit as its parent, and
    # this fires -- every round it happens, with no history to diff against
    # and no saturation.
    tips_now = tips()
    for name, tip in tips_now.items():
        was = tips_before.get(name)
        if was is None or tip == was or was == "0":
            continue
        obj = load(tip)
        if obj is None:
            continue
        parents = obj.get("parents") or []
        if was not in parents:
            misparented.append((i, name, tip, was))
            print(
                f"  MISPARENTED round {i}: '{name}' advanced to {tip[:8]} whose parents "
                f"are {[p[:8] for p in parents] or 'none'}, not its own previous tip {was[:8]}"
            )

    now = {name: reachable(tip) for name, tip in tips_now.items()}
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

print(f"  {overlapped}/{rounds} rounds actually overlapped in wall time "
      f"({both_ok} of those also had both commands exit 0)")
print(f"  RESULT: {len(misparented)} misparented, {len(lost)} lost, "
      f"{len(mismatched)} mismatched, {len(inconsistent)} inconsistent")
if misparented and not lost:
    print("  note: `lost` saturates and can read 0 while the race fires every round; "
          "`misparented` is the rate.")
if misparented or lost or mismatched or inconsistent:
    sys.exit(1)
if overlapped == 0:
    print("  INCONCLUSIVE: the two commands never overlapped -- adjust the delay, "
          "including its sign, and re-run. A green here proves nothing.")
    sys.exit(2)
if not REACHABLE:
    print(f"  INCONCLUSIVE: nothing was found, but {DELAY:+.3f}s is outside the range "
          f"that can reach the window (below {USABLE_BELOW:+.3f}s on this build), so a "
          f"clean result was the only possible outcome. Re-run with a smaller or "
          f"negative delay before treating this as a pass.")
    sys.exit(2)
