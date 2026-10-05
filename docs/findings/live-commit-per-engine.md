# What a `gfs commit` promises, per database engine

Measured on the Kubernetes runtime (k3s + openebs-zfs VolumeSnapshot), October 2026.

This exists because one comment written for PostgreSQL was being read as though it
covered every provider, and a second comment on ClickHouse described the Docker
runtime's behaviour while sitting on a path that does not pause anything.

## The one thing that actually makes a commit sound

**Snapshot atomicity, not the restore looking healthy.**

A naive, non-atomic copy of a live datadir starts, answers queries and passes the
engine's own structural check. That was measured for MySQL: a plain copy taken
while writes were in flight produced a server that came up, held plausible rows
and returned `CHECK TABLE … OK`. Nothing in the restored image announced that it
was torn.

So a green structural check is not evidence of a sound commit. The argument has
to rest on the snapshot being atomic at the storage layer.

| Runtime | `db_live_during_snapshot` | What makes it sound |
| --- | --- | --- |
| Kubernetes | `true` (`compute-kubernetes/src/lib.rs`) | VolumeSnapshot is atomic (ZFS txg); the database keeps serving |
| Docker | `false` (`compute-docker/src/lib.rs`) | the container is paused around the snapshot |

The APFS storage adapter clones per file via `clonefile(2)`. Each file is atomic;
a tree of files is **not** atomic with respect to one another. That distinction is
the whole reason the Kubernetes path can commit live and a plain copy cannot.

## Per engine

### PostgreSQL

Has a real protocol for this: `pg_backup_start` / `pg_backup_stop` in one session,
with the post-stop WAL and `backup_label` retained. A restore then replays WAL and
*says so* — `redo starts at`, `consistent recovery state reached` — so PostgreSQL
is the one engine where the restore carries its own evidence.

### PostgreSQL on Kubernetes, measured

The section above describes the Docker recipe. On Kubernetes the atomic
`VolumeSnapshot` makes it unnecessary, and that is now measured rather than
assumed — `postgres-live-commit-spike.md` recommended testing this first
precisely because it removes a pause instead of adding a mechanism.

Conditions were the unforgiving ones: `data_checksums off` and `wal_keep_size 0`,
so a torn snapshot's corruption would not be detectable. A writer inserted 400
rows per second throughout.

```
C1 before commit  15800
C2 after commit   16200     <- writes never paused
restored          16200     <- inside the bracket, at a single instant
max(id)           16200
count(distinct id) 16200
```

The restored cluster's log:

```
database system was interrupted; last known up at 15:11:18 UTC
database system was not properly shut down; automatic recovery in progress
redo starts at 0/17A4258
invalid record length at 0/17B4930: expected at least 24, got 0
redo done at 0/17B4908
database system is ready to accept connections
```

**Read that `invalid record length` carefully.** It is the same line the spike
flagged on the torn copy, and here it is benign: in crash recovery it is how
Postgres finds the end of the log, and it appears at the WAL *tail*, immediately
before `redo done`. The spike's failure was the same message occurring *early* —
recovery stopping at a torn record with committed data beyond it. The message
alone does not tell the two apart; what does is whether the restored data lands
at one instant, which the bracket above shows.

Note what the log does **not** say: `consistent recovery state reached`. That
line belongs to base-backup recovery, driven by a `backup_label`. An atomic
snapshot produces ordinary *crash* recovery instead, which ends at `redo done`.
Anything specifying the Kubernetes path should expect the crash-recovery
signature, not the base-backup one.

`bt_index_parent_check` also passed, and that is recorded here only for
completeness: the spike's deliberately torn copy passed it too, so it is not
evidence either way. The load-bearing facts are the single-instant snapshot and
the completed recovery.

Consequence for the engine-assisted backup window: it is needed for the
**non-atomic** adapters (`storage-apfs`, `storage-file`), where a tree walk sees
different files at different instants. On `storage-kubernetes` and
`storage-btrfs` the snapshot is already one instant, so `pg_backup_start` /
`pg_backup_stop` would add machinery without adding safety.

### MySQL

Restores from an atomic snapshot consistently, but **reports nothing either way**.
At default verbosity InnoDB logs only `InnoDB initialization has started` /
`ended`, whether or not it recovered. Measured: 10,771 rows restored from inside a
10,655..10,809 commit window, `CHECK TABLE … OK`, and no crash-recovery line in
the log — and the deliberately torn copy produced exactly the same signals.

Consequence: for MySQL, do not look for recovery evidence in the log, and do not
treat `CHECK TABLE` as discriminating. Only snapshot atomicity is load-bearing.

### ClickHouse

Commits live on Kubernetes, and unlike MySQL its structural check **does**
discriminate, because every part carries `checksums.txt`.

Measured, with a one-second-resolution sampler running inside the pod:

```
1791207393  48600
1791207394  49000   <- commit start
1791207395  49400   <- commit end
1791207396  49800
```

1,200 rows landed across the commit window — the database was never paused. The
restore of that commit returned **49,400** rows, inside the window and exactly the
count sampled at the commit-end second, with `CHECK TABLE t` = 1 and 2 active
parts.

Calibrated: corrupting 512 bytes of one part's `data.bin` turns the same check
red, naming the part and the mismatch:

```
all_1_111_22    1
all_112_112_0   0   Checksum mismatch for file data.bin in data part
                    all_112_112_0 (CHECKSUM_DOESNT_MATCH)
```

So for ClickHouse the check is not a no-op. Note the limit of that calibration: it
proves the check detects **corruption within a part**, not that it would detect a
**torn set of parts**, where each part is individually intact. Tearing across parts
remains something only snapshot atomicity rules out.

No engine-side flush was needed: the atomic snapshot alone produced a consistent
restore, so `prepare_for_snapshot` correctly returns no commands for ClickHouse —
but for the stated reason, not the one the old comment gave.
