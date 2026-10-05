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
