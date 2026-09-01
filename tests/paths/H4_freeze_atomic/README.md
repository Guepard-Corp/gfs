# H4: a freeze killed mid-flight rolls back completely; the clone stays usable

**Related issues:** [#132](https://github.com/Guepard-Corp/gfs/issues/132)

## Why this test exists

H4 (#132): freeze is ONE transaction. Killed mid-flight, EVERYTHING rolls
back -- the truncates, the copies, the bookkeeping and the frozen flag -- and
the clone is exactly the working lazy clone it was. The freeze backend is
found by application_name ('gfs_freeze', set by gfs.freeze_run itself), and
held mid-flight deterministically by an ACCESS EXCLUSIVE lock it must wait on.

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 5000 rows, big enough that a whole copy
is visible in the clone's own statistics
other(id, v) with 1 row
```

The source is not modified: this test is about what the clone does on its own.

## What is asserted

- the freeze backend is visible mid-flight (application_name=gfs_freeze)
- the frozen flag rolled back
- coverage bookkeeping rolled back (orders still whole-cached)
- no rows were lost to the aborted truncate
- a later freeze succeeds cleanly
- and the frozen clone answers offline

## Running it

```bash
tests/paths/run-all.sh H4          # through the runner
bash tests/paths/H4_freeze_atomic/test.sh   # directly
```

Each test builds its **own** throwaway source and its **own** clone, so it can be
run alone and cannot be affected by any other test.

## Harness helpers used

| helper | what it does |
| --- | --- |
| `P` | runs SQL directly inside the clone container |
| `clone_now` | clones the source and waits until the clone is queryable |
| `clone_state` | reads a column of gfs.clone_source for one table |
| `val` | reads through the gfs CLI and returns one value |
| `wait_until_cached` | polls until the table is genuinely held whole locally |
| `with_source_down` | evaluates a query with the source stopped, then restarts it |

See [`../lib/common.sh`](../lib/common.sh) for the harness, and
[`../README.md`](../README.md) for the traps that have produced false results here.
