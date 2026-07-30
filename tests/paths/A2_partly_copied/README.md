# A2: a partly copied table still answers correctly for everything

## Why this test exists

A2: a partly copied table. The clone holds some rows locally and must still
answer correctly for the rows it does not hold.

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 5000 rows, big enough that a whole copy
is visible in the clone's own statistics
other(id, v) with 1 row
```

The source is not modified: this test is about what the clone does on its own.

## What is asserted

- the full count is right regardless of what is held locally
- a slice never touched locally is still correct

## Running it

```bash
tests/paths/run-all.sh A2          # through the runner
bash tests/paths/A2_partly_copied/test.sh   # directly
```

Each test builds its **own** throwaway source and its **own** clone, so it can be
run alone and cannot be affected by any other test.

## Harness helpers used

| helper | what it does |
| --- | --- |
| `clone_now` | clones the source and waits until the clone is queryable |
| `val` | reads through the gfs CLI and returns one value |

See [`../lib/common.sh`](../lib/common.sh) for the harness, and
[`../README.md`](../README.md) for the traps that have produced false results here.
