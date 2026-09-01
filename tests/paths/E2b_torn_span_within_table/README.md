# E2b: one table copied chunk-by-chunk reports the moments its rows span

**Related issues:** [#131](https://github.com/Guepard-Corp/gfs/issues/131)

## Why this test exists

E2b (#131): the within-table variant. An int-range key hydrates KEY RANGES at
different moments into ONE table; the per-table min..max watermark plus a
moment count make that reportable ("rows copied across N moments") -- the
thing a single per-table timestamp could never express. Which ROW came from
which moment stays unknowable (ranges coalesce, and ON CONFLICT DO NOTHING
keeps older row versions silently), so the span is the honest ceiling, and
this test pins exactly that ceiling.

The source is moved by writing to a DIFFERENT table (`other`), which is the
only way to get two copy moments INSIDE one table: writing to `orders` itself
would flag it, and a flagged table federates rather than copies, so the second
read would stamp nothing at all (see E1b). Moving `other` advances the
source-wide totals -- the moment identity -- while leaving `orders` on the
lazy path, so its second chunk is a genuine copy event at a later moment.

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 5000 rows, big enough that a whole copy
is visible in the clone's own statistics
other(id, v) with 1 row
```

**Then the source does:**

```sql
INSERT INTO other VALUES (2,'b');
```

## What is asserted

- gfs fetch reports the span
- expected moments >= 2 on orders, got '$M'

## Running it

```bash
tests/paths/run-all.sh E2b          # through the runner
bash tests/paths/E2b_torn_span_within_table/test.sh   # directly
```

Each test builds its **own** throwaway source and its **own** clone, so it can be
run alone and cannot be affected by any other test.

## Harness helpers used

| helper | what it does |
| --- | --- |
| `P` | runs SQL directly inside the clone container |
| `clone_now` | clones the source and waits until the clone is queryable |
| `clone_state` | reads a column of gfs.clone_source for one table |
| `src` | runs SQL on the SOURCE database |
| `val` | reads through the gfs CLI and returns one value |

See [`../lib/common.sh`](../lib/common.sh) for the harness, and
[`../README.md`](../README.md) for the traps that have produced false results here.
