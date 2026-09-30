#!/usr/bin/env python3
"""List, and optionally delete, snapshot trees no commit refers to.

Usage: gfs-reclaim-orphan-snapshots.py <repo> [--delete]

A commit takes its snapshot BEFORE it writes the commit object, and the
destination is derived from the workspace path and a timestamp rather than from
the content — so it is known, and created, up front. Killing `gfs commit`
between those two points (SIGKILL, a lost SSH session, a machine losing power)
therefore leaves a complete-looking snapshot tree under `.gfs/snapshots/` that
nothing references. Nothing is corrupt: the lock is released by the kernel and
the next commit works. The tree is simply never read again, and never freed.

GFS has no `gc`, so this is the reclaim. It is deliberately a separate script
rather than part of commit: deleting data is not something a commit should do
on its own initiative, and the operator should see what is about to go.

An orphan is a directory under `.gfs/snapshots/<2>/<rest>` whose hash is not the
`snapshot_hash` of any object under `.gfs/objects/`. Every commit is scanned,
not just the ones reachable from a ref, so a snapshot belonging to a commit on
a deleted branch is NOT reclaimed — the safe direction, and the one that keeps
this usable without knowing how the caller manages refs.

Prints what it finds and exits 0. With --delete, removes the orphans; snapshot
trees are read-only by design, so their permissions are restored first.
"""
import json
import os
import shutil
import stat
import sys

if len(sys.argv) < 2:
    sys.exit(__doc__)
repo = sys.argv[1]
delete = "--delete" in sys.argv[2:]

gfs = os.path.join(repo, ".gfs")
if not os.path.isdir(gfs):
    sys.exit(f"not a GFS repository: {repo}")


def two_level(root):
    """Yield (hash, path) for every `<root>/<2 chars>/<rest>` entry."""
    if not os.path.isdir(root):
        return
    for prefix in sorted(os.listdir(root)):
        sub = os.path.join(root, prefix)
        if len(prefix) != 2 or not os.path.isdir(sub):
            continue
        for rest in sorted(os.listdir(sub)):
            yield prefix + rest, os.path.join(sub, rest)


referenced = set()
damaged = []
for _, path in two_level(os.path.join(gfs, "objects")):
    try:
        with open(path) as fh:
            obj = json.load(fh)
    except (OSError, ValueError):
        # An object that cannot be parsed might be the only reference to a
        # snapshot, so it has to make this MORE conservative, not less.
        # `continue` did the opposite: it contributed nothing to `referenced`,
        # so a snapshot only that object referred to became an orphan and
        # `--delete` would destroy it. Measured on a repository with one commit
        # object corrupted in place and its refs intact: referenced went 4 -> 3
        # and the live snapshot was listed for deletion.
        #
        # But refusing on every unparseable object is also wrong: a healthy
        # repository contains objects that are legitimately not JSON at all --
        # the files object is binary, and `two_level` also yields directories.
        # Refusing on those would mean refusing on every repository.
        #
        # So the discriminator is whether it LOOKS like a commit: commit objects
        # are JSON and start with `{`. One that starts with `{` and will not
        # parse is damaged, and we stop. Anything else is another object kind
        # and is skipped as before.
        if os.path.isdir(path):
            continue
        try:
            with open(path, "rb") as fh:
                head = fh.read(64).lstrip()
        except OSError:
            head = b"{"  # cannot even read it: treat as damaged
        if head.startswith(b"{"):
            damaged.append(path)
        continue
    snapshot = obj.get("snapshot_hash")
    if snapshot:
        referenced.add(snapshot)

if damaged:
    print(
        f"refusing to reclaim: {len(damaged)} commit-shaped object(s) could not be parsed, "
        f"so the set of referenced snapshots is not known and any snapshot could be one "
        f"of theirs.",
        file=sys.stderr,
    )
    for path in damaged[:10]:
        print(f"  {path}", file=sys.stderr)
    sys.exit(2)


def tree_size(path):
    total = 0
    for base, _, files in os.walk(path):
        for name in files:
            try:
                total += os.lstat(os.path.join(base, name)).st_size
            except OSError:
                pass
    return total


def human(n):
    for unit in ("B", "KB", "MB", "GB", "TB"):
        if n < 1024 or unit == "TB":
            return f"{n:.0f} {unit}" if unit == "B" else f"{n:.1f} {unit}"
        n /= 1024


orphans = [(h, p) for h, p in two_level(os.path.join(gfs, "snapshots"))
           if h not in referenced]
snapshots = list(two_level(os.path.join(gfs, "snapshots")))

print(f"{len(snapshots)} snapshot(s), {len(referenced)} referenced by a commit")
if not orphans:
    print("no orphans")
    sys.exit(0)

reclaimable = 0
for h, path in orphans:
    size = tree_size(path)
    reclaimable += size
    print(f"  orphan {h[:12]}  {human(size):>10}  {path}")
print(f"reclaimable: {human(reclaimable)}")

if not delete:
    print("re-run with --delete to remove them")
    sys.exit(0)

for h, path in orphans:
    for base, dirs, files in os.walk(path):
        for name in dirs + files:
            target = os.path.join(base, name)
            try:
                os.chmod(target, os.lstat(target).st_mode | stat.S_IWUSR)
            except OSError:
                pass
    os.chmod(path, os.lstat(path).st_mode | stat.S_IWUSR)
    shutil.rmtree(path)
    print(f"  removed {h[:12]}")
print(f"reclaimed {human(reclaimable)}")
