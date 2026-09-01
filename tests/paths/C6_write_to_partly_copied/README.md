# C6: writing to a partly copied table completes the copy first

## Why this test exists

C6: you write to a table only partly copied. The write must land on complete
data, so the table is filled in first rather than accepting a write against a
fragment.

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 5000 rows, big enough that a whole copy
is visible in the clone's own statistics
other(id, v) with 1 row
```

The source is not modified: this test is about what the clone does on its own.

## What is asserted

- all 5000 source rows plus the local one
- the local row is present

## Running it

```bash
tests/paths/run-all.sh C6          # through the runner
bash tests/paths/C6_write_to_partly_copied/test.sh   # directly
```

Each test builds its **own** throwaway source and its **own** clone, so it can be
run alone and cannot be affected by any other test.

## Harness helpers used

| helper | what it does |
| --- | --- |
| `clone_now` | clones the source and waits until the clone is queryable |
| `q` | reads through the gfs CLI and returns the whole output |
| `val` | reads through the gfs CLI and returns one value |

See [`../lib/common.sh`](../lib/common.sh) for the harness, and
[`../README.md`](../README.md) for the traps that have produced false results here.
