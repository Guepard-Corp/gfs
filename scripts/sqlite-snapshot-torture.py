#!/usr/bin/env python3
"""Commit repeatedly while the database is written continuously.

Usage: sqlite-snapshot-torture.py <repo> <gfs-binary> <rounds> [relative-db-path]

`<repo>` must already be an initialised GFS repo using the sqlite provider. The
`ledger` table this writes to is created here if it does not exist, so the
script is runnable exactly as the usage line reads.

The writer runs in a thread of THIS process, but each `gfs commit` is a
subprocess, so the contention under test is genuinely cross-process — it is
SQLite's file locking being exercised, not Python's GIL. The writer is a
separate connection either way; the thread is a convenience for reading the
row count, not part of what is under test.

Each commit must capture a snapshot that:

  (a) passes PRAGMA integrity_check;
  (b) holds a row count between the counts observed immediately before and
      after the commit — a state the database genuinely passed through;
  (c) contains only whole transactions.

The third check is the one that matters, and the reason it is not simply a row
count. A torn transaction leaves a database that is STRUCTURALLY valid, so
integrity_check calls it ok, and whose row count can still land inside the
window. Every transaction here inserts exactly GROUP rows stamped with one
increasing batch number, so a snapshot is whole only when every batch it
contains has all GROUP of its rows and the batches run 1..max with no gap. A
count-only check would have to rely on a partial batch failing to be a multiple
of GROUP, which is a 1-in-GROUP coincidence away from passing.

The optional fourth argument is the database path relative to the workspace,
for layouts that do not use the conventional name — Rails 7.1 keeps it at
`storage/development.sqlite3`, in a subdirectory.

Commits are mapped to snapshots through the commit object's `snapshot_hash`.
Sorting snapshot directories by mtime does not work: they are made read-only at
creation and share a timestamp.

Exit status is 0 only if no round failed. A REFUSED commit is not a failure —
declining to snapshot a database it cannot quiesce is the correct outcome — but
a run that is entirely refusals proves nothing, and says so.
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

os.makedirs(os.path.dirname(db), exist_ok=True)
setup = sqlite3.connect(db, timeout=60)
setup.executescript(
    "PRAGMA journal_mode=WAL;"
    "CREATE TABLE IF NOT EXISTS ledger("
    "  id INTEGER PRIMARY KEY, batch INTEGER NOT NULL, payload TEXT NOT NULL);"
    "CREATE INDEX IF NOT EXISTS ledger_batch ON ledger(batch);"
)
setup.close()


def count():
    c = sqlite3.connect(db, timeout=60)
    try:
        return c.execute("SELECT count(*) FROM ledger").fetchone()[0]
    finally:
        c.close()


stop = threading.Event()
wrote = [0]
batch = [0]


def writer():
    w = sqlite3.connect(db, timeout=60)
    w.execute("PRAGMA journal_mode=WAL")
    while not stop.is_set():
        n = batch[0] + 1
        try:
            w.execute("BEGIN IMMEDIATE")
            for _ in range(GROUP):
                w.execute("INSERT INTO ledger(batch, payload) VALUES(?, 'y')", (n,))
            w.commit()
            batch[0] = n
            wrote[0] += GROUP
        except sqlite3.OperationalError:
            time.sleep(0.01)
    w.close()


def snapshot_for(commit_hash):
    """Resolve a commit to its snapshot directory via the commit object."""
    obj = os.path.join(repo, ".gfs", "objects", commit_hash[:2], commit_hash[2:])
    h = json.load(open(obj))["snapshot_hash"]
    return os.path.join(repo, ".gfs", "snapshots", h[:2], h[2:])


def torn_batches(conn):
    """Batches that are not whole: a short one, or a gap in the sequence.

    Returns a short description, or None when every transaction in the snapshot
    is complete.
    """
    short = conn.execute(
        "SELECT batch, count(*) FROM ledger GROUP BY batch HAVING count(*) <> ?",
        (GROUP,),
    ).fetchall()
    if short:
        return "partial batch %s (%d of %d rows)" % (short[0][0], short[0][1], GROUP)
    row = conn.execute("SELECT count(DISTINCT batch), max(batch) FROM ledger").fetchone()
    distinct, highest = row[0], row[1]
    if highest is not None and distinct != highest:
        return "batch gap: %d distinct batches but highest is %d" % (distinct, highest)
    return None


th = threading.Thread(target=writer, daemon=True)
th.start()
time.sleep(2)

print("  %-7s %-12s %-12s %-12s %-10s %s"
      % ("commit", "before", "after", "snapshot", "integrity", "verdict"))
passed = failed = refused = 0
for i in range(1, rounds + 1):
    before = count()
    r = subprocess.run([gfs, "commit", "-m", f"t{i}"], cwd=repo,
                       capture_output=True, text=True, timeout=300)
    after = count()
    if r.returncode != 0:
        refused += 1
        msg = (r.stderr or r.stdout).strip().splitlines()
        print("  %-7s %-12s %-12s %s"
              % (i, before, after, "REFUSED: " + (msg[0][:70] if msg else "?")))
        continue

    head = open(os.path.join(repo, ".gfs", "refs", "heads", "main")).read().strip()
    snap_dir = snapshot_for(head)
    tmp = tempfile.mkdtemp()
    shutil.copytree(snap_dir, os.path.join(tmp, "snap"))
    s = sqlite3.connect(os.path.join(tmp, "snap", REL))
    integ = s.execute("PRAGMA integrity_check").fetchone()[0]
    rows = s.execute("SELECT count(*) FROM ledger").fetchone()[0]
    torn = torn_batches(s)
    s.close()
    shutil.rmtree(tmp, ignore_errors=True)

    if integ != "ok":
        verdict = "FAIL (corrupt)"
    elif torn:
        verdict = "FAIL (torn transaction: %s)" % torn
    elif not before <= rows <= after:
        verdict = "FAIL (state that never existed)"
    else:
        verdict = "pass"
    passed, failed = ((passed + 1, failed) if verdict == "pass"
                      else (passed, failed + 1))
    print("  %-7s %-12s %-12s %-12s %-10s %s"
          % (i, before, after, rows, integ, verdict))

stop.set()
th.join(timeout=10)
print(f"  writer inserted {wrote[0]} rows in {batch[0]} transactions")
print(f"  RESULT: {passed} passed, {failed} failed, {refused} refused")
if failed:
    sys.exit(1)
if not passed:
    print("  INCONCLUSIVE: no commit produced a snapshot to check")
    sys.exit(2)
