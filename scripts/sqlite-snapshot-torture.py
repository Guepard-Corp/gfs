#!/usr/bin/env python3
"""Commit repeatedly while the database is written continuously.

Usage: sqlite-snapshot-torture.py <repo> <gfs-binary> <rounds> [relative-db-path]

The writer runs in a thread of THIS process, but each `gfs commit` is a
subprocess, so the contention under test is genuinely cross-process — it is
SQLite's file locking being exercised, not Python's GIL.

Each commit must capture a snapshot that (a) passes PRAGMA integrity_check,
(b) holds a row count between the counts observed immediately before and after
the commit — a state the database genuinely passed through — and (c) contains
only whole transactions. The third check matters: a torn transaction leaves a
database that is structurally valid, so integrity_check calls it ok, and whose
row count can still fall inside the window. Writing in fixed-size groups makes
that detectable as a row count that is not a multiple of the group size.

The optional fourth argument is the database path relative to the workspace,
for layouts that do not use the conventional name — Rails 7.1 keeps it at
`storage/development.sqlite3`, in a subdirectory.

Commits are mapped to snapshots through the commit object's `snapshot_hash`.
Sorting snapshot directories by mtime does not work: they are made read-only at
creation and share a timestamp.
"""
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time

repo, gfs, rounds = sys.argv[1], sys.argv[2], int(sys.argv[3])
REL = sys.argv[4] if len(sys.argv) > 4 else "db.sqlite"
GROUP = 300
db = os.path.join(open(os.path.join(repo, ".gfs", "WORKSPACE")).read().strip(), REL)


def count():
    c = sqlite3.connect(db, timeout=60)
    try:
        return c.execute("SELECT count(*) FROM ledger").fetchone()[0]
    finally:
        c.close()


stop = threading.Event()
wrote = [0]


def writer():
    w = sqlite3.connect(db, timeout=60)
    w.execute("PRAGMA journal_mode=WAL")
    while not stop.is_set():
        try:
            w.execute("BEGIN IMMEDIATE")
            for _ in range(GROUP):
                w.execute("INSERT INTO ledger(payload) VALUES('y')")
            w.commit()
            wrote[0] += GROUP
        except sqlite3.OperationalError:
            time.sleep(0.01)
    w.close()


def snapshot_for(commit_hash):
    """Resolve a commit to its snapshot directory via the commit object."""
    obj = os.path.join(repo, ".gfs", "objects", commit_hash[:2], commit_hash[2:])
    h = json.load(open(obj))["snapshot_hash"]
    return os.path.join(repo, ".gfs", "snapshots", h[:2], h[2:])


th = threading.Thread(target=writer, daemon=True)
th.start()
time.sleep(2)

print("  %-7s %-12s %-12s %-12s %-10s %s" % ("commit", "before", "after", "snapshot", "integrity", "verdict"))
passed = failed = refused = 0
for i in range(1, rounds + 1):
    before = count()
    r = subprocess.run([gfs, "commit", "-m", f"t{i}"], cwd=repo,
                       capture_output=True, text=True, timeout=300)
    after = count()
    if r.returncode != 0:
        refused += 1
        msg = (r.stderr or r.stdout).strip().splitlines()
        print("  %-7s %-12s %-12s %s" % (i, before, after, "REFUSED: " + (msg[0][:70] if msg else "?")))
        continue

    head = open(os.path.join(repo, ".gfs", "refs", "heads", "main")).read().strip()
    snap_dir = snapshot_for(head)
    tmp = tempfile.mkdtemp()
    shutil.copytree(snap_dir, os.path.join(tmp, "snap"))
    s = sqlite3.connect(os.path.join(tmp, "snap", REL))
    integ = s.execute("PRAGMA integrity_check").fetchone()[0]
    rows = s.execute("SELECT count(*) FROM ledger").fetchone()[0]
    s.close()
    shutil.rmtree(tmp, ignore_errors=True)

    whole = rows % GROUP == 0
    ok = integ == "ok" and before <= rows <= after and whole
    passed, failed = (passed + 1, failed) if ok else (passed, failed + 1)
    print("  %-7s %-12s %-12s %-12s %-10s %s" % (i, before, after, rows, integ,
                                                 "pass" if ok else
                                                 ("FAIL (torn transaction)" if not whole
                                                  else "FAIL (state that never existed)")))

stop.set()
th.join(timeout=10)
print(f"  writer inserted {wrote[0]} rows")
print(f"  RESULT: {passed} passed, {failed} failed, {refused} refused")
sys.exit(1 if failed else 0)
