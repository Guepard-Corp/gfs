# F1b: housekeeping WAL movement does not mark every copied table suspect

**Related issues:** [#119](https://github.com/Guepard-Corp/gfs/issues/119), [#129](https://github.com/Guepard-Corp/gfs/issues/129), [#140](https://github.com/Guepard-Corp/gfs/issues/140)

## Why this test exists

F1b (#140): an idle PostgreSQL advances its own WAL -- checkpointer, autovacuum,
the stats collector. That movement is indistinguishable from a real change by
LSN alone, so it used to fire the unattributed blanket and mark EVERY copied
table suspect. The router computes `stale = drifted && !diverged`, so a
blanket-marked table cannot be served locally even when the rows are held: the
clone federates every read and offline reads stop working. That is #119's
failure reached through a different door.

The blanket itself must stay -- #129 exists because it was once suppressed too
eagerly. What separates the two cases is whether any row actually changed
anywhere on the source, which F1c pins from the other side.

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 3 rows: Alice 50, Bob 30, Carol 20
notes(id, body) with 1 row
other(id, v) with 1 row, used only to trigger background checks
```

**Then the source does:**

```sql
CHECKPOINT;
```

## What is asserted

- the clone still answers from its own copy
- the source's WAL moved with no user write ($BEFORE -> $AFTER)

## Running it

```bash
tests/paths/run-all.sh F1b          # through the runner
bash tests/paths/F1b_benign_wal_movement/test.sh   # directly
```

Each test builds its **own** throwaway source and its **own** clone, so it can be
run alone and cannot be affected by any other test.

## Harness helpers used

| helper | what it does |
| --- | --- |
| `P` | runs SQL directly inside the clone container |
| `clone_now` | clones the source and waits until the clone is queryable |
| `nudge` | reads an unrelated table so a background drift check can run and commit |
| `src` | runs SQL on the SOURCE database |
| `val` | reads through the gfs CLI and returns one value |

See [`../lib/common.sh`](../lib/common.sh) for the harness, and
[`../README.md`](../README.md) for the traps that have produced false results here.
