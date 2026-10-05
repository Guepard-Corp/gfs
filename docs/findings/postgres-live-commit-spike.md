# Can `commit` snapshot a live Postgres without pausing it?

**Measured on 2026-10-04, PostgreSQL 17.11 (aarch64), Docker.** Writers ran
continuously throughout every attempt. Nothing below is relayed from
documentation; each result is a log line from a cluster that was actually
started.

**Answer: yes on Docker, with a recipe that has three preconditions — and
probably already yes on Kubernetes for a different reason.** The two runtimes
need different work because their snapshots differ in atomicity, not in
database behaviour.

## Why the runtime decides this

| Storage adapter | Mechanism | Atomic per file | Atomic across the tree |
| --------------- | --------- | --------------- | ---------------------- |
| `storage-apfs` | `cp -cRp` → `clonefile(2)` | **Yes** — instant CoW, zero unique bytes | No — `cp` recurses, one clone per file |
| `storage-file` | `cp -cRp` | Yes on APFS, plain copy elsewhere | No |
| `storage-btrfs` | `btrfs subvolume snapshot` | — | **Yes** — one call, one instant |
| `storage-kubernetes` | `VolumeSnapshot` (openebs-zfs) | — | **Yes** |

APFS does do copy-on-write, and each `clonefile(2)` is atomic and instant.
What `cp -cRp` does not give is a single instant across a *set* of files: it
recurses, so file A is cloned at T1 and file B at T2, and a writer touching
both in between leaves the copy holding old-A with new-B.

APFS has tree-wide atomic snapshots (`fs_snapshot_create`, what
`tmutil localsnapshot` drives) but they are **volume-scoped**, and there is no
directory-granular snapshot API. gfs needs per-repository granularity — many
repositories live as directories under one filesystem — so per-file cloning is
the only mechanism matching the layout, short of a volume per repository. The
choice is forced, not an oversight.

A tree walk sees different files at different instants, so the copy is torn by
construction and the container pause is what currently makes it safe. An atomic
volume snapshot is equivalent to a power cut at one instant, which Postgres
crash recovery is designed for — so on Kubernetes and btrfs the pause may
already be unnecessary. That is the cheaper of the two improvements and it is
untested here.

## The Docker recipe, and the two ways it fails first

Three attempts, each with a live writer:

| Attempt | Setup | Result |
| ------- | ----- | ------ |
| 1 | `pg_backup_start` → `cp -a` PGDATA (incl. `pg_wal`) → `pg_backup_stop` | **`FATAL: WAL ends before end of online backup`**, invalid record at `0/2003768` |
| 2 | as above, plus WAL copied in *after* `pg_backup_stop` | **Same FATAL**, now at `0/3741510` — further along, still short |
| 3 | `wal_keep_size=1GB`, WAL copied in after stop, `backup_label` installed | **Consistent.** `completed backup recovery with redo LSN 0/6000BF8 and end LSN 0/600FD38` → `consistent recovery state reached` → `ready to accept connections` |

Attempt 1 fails because copying `pg_wal` as part of the tree walk captures it
*before* the window closes. Attempt 2 fails because the writer recycles WAL
segments, so the segments the backup needs are overwritten before they can be
shipped. Both are silent until restore.

So the recipe has three preconditions, none optional:

1. `pg_backup_start()` and `pg_backup_stop()` **in the same session** — the
   backup is aborted if the connection drops, so this cannot be two separate
   `exec` calls.
2. **WAL retained across the window** — `wal_keep_size`, a replication slot, or
   archiving. Without it the needed segments are recycled.
3. **WAL shipped into the copy after `pg_backup_stop()` returns**, plus the
   returned `backup_label` written into the copy and `postmaster.pid` removed.

Postgres says precondition 2 out loud at `pg_backup_start`:
`WAL archiving is not enabled; you must ensure that all required WAL segments
are copied through other means to complete the backup`.

Verified on the restored cluster: 108,860 rows against a live source then at
117,460 — a consistent point *inside* the window, not the live tip — all `id`s
distinct, index scans usable, `bt_index_parent_check` clean.

## The negative control is the reason to do this properly

A naive copy of a live PGDATA with no backup window at all **started
successfully**, answered queries (126,800 rows), passed
`bt_index_parent_check`, and completed a full heap scan. Its log shows
`invalid record length at 0/72A34C8` — recovery stopped at a torn record,
treated it as end of WAL, and came up anyway.

So the naive path is **not reliably detectable as broken**. It depends on copy
duration, which pages were in flight, and whether `full_page_writes` happened
to cover them; `data_checksums` is `off` on this image, so a torn page past the
replayed WAL would not be caught at all. That is the worst possible failure
shape for this product: it passes in testing and loses data in production under
load. The argument for the recipe is not that the naive copy visibly broke —
it is that it did not.

## What this means for gfs

- `prepare_for_snapshot` is already the right seam and already per-provider. On
  `main` it is `CHECKPOINT;` for postgres and **empty** for mysql and
  clickhouse, so the container pause is doing all the consistency work for two
  of four providers.
- The Postgres provider cannot express this recipe through
  `prepare_for_snapshot` alone, because the contract returns a list of commands
  run *before* the snapshot. This needs a **session held open across the
  snapshot** and a **post-snapshot step**. That is a port change: the shape
  `LocalEngine` already has — `prepare_for_snapshot` returning a guard whose
  `Drop` ends the window — is what the container path needs too.
- SQLite reaches the same place by a different route: its `LocalEngine`
  implementation holds the write lock, which is the embedded equivalent of the
  pause. Turso's WAL watermark is the version of this idea that needs no lock
  at all, but the engine is beta.

## Recommended order

1. **Kubernetes/btrfs first.** Test whether an atomic `VolumeSnapshot` of a live
   PGDATA restores consistently with no pause and no backup window. If yes, the
   win is removing a pause, not adding a mechanism — much less code.
2. **Docker second**, with the three-precondition recipe, which requires the
   port change above.
3. **MySQL and ClickHouse** after, where `prepare_for_snapshot` is empty today:
   ClickHouse's immutable parts (`ALTER TABLE … FREEZE`) should be the easiest
   of the four.

## Reproduction

Container `gfs-spike-src` (`postgres:17`), writer inserting 20 rows per
statement in a loop. Copies at `/var/lib/postgresql/copy` (attempt 1),
`copy3` (attempt 3, port 5434) and `naive` (control, port 5435). Removed after
the run; the commands are in this document's history.
