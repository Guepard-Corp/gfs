# E2: one table can reflect two different moments

> **This test is expected to FAIL.** It is marked `--expect open`, which means it
> asserts what the system does *today* for something that is still unfixed. The
> suite counts it as passing. If it ever starts passing on its own, the runner
> exits 3 and tells you the documentation is out of date.

**Related issues:** [#131](https://github.com/Guepard-Corp/gfs/issues/131), [#132](https://github.com/Guepard-Corp/gfs/issues/132)

## Why this test exists

E2 (#131): the same tear, inside a single table.

Two halves of one table are observed at different moments. A point-in-time
clone would show both halves as of ONE instant. Declared KNOWN-OPEN, and it
stays that way on purpose: a LAZY clone cannot fix this -- the source has
already thrown the old row versions away -- so this path keeps asserting what
actually happens rather than what we wish did.

What IS fixed is the SILENCE. E2b proves the table's own watermark counts the
moments its rows span, and H8 proves `gfs freeze` (#132) collapses the clone
back to a single moment. This path is the tear; those two are the awareness
and the cure.

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 5000 rows, big enough that a whole copy
is visible in the clone's own statistics
other(id, v) with 1 row
```

**Then the source does:**

```sql
INSERT INTO orders SELECT g,'late'||g,g FROM generate_series(9000,9100) g;
```

## What is asserted

- the table now reflects a later moment ($TOT rows) than the one first read (#131, fixed by #132)

## Running it

```bash
tests/paths/run-all.sh E2          # through the runner
bash tests/paths/E2_torn_within_table/test.sh   # directly
```

Each test builds its **own** throwaway source and its **own** clone, so it can be
run alone and cannot be affected by any other test.

## Harness helpers used

| helper | what it does |
| --- | --- |
| `clone_now` | clones the source and waits until the clone is queryable |
| `nudge` | reads an unrelated table so a background drift check can run and commit |
| `src` | runs SQL on the SOURCE database |
| `val` | reads through the gfs CLI and returns one value |

See [`../lib/common.sh`](../lib/common.sh) for the harness, and
[`../README.md`](../README.md) for the traps that have produced false results here.
