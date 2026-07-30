# E1: a clone changes under you: the same query gives different answers over time

> **This test is expected to FAIL.** It is marked `--expect open`, which means it
> asserts what the system does *today* for something that is still unfixed. The
> suite counts it as passing. If it ever starts passing on its own, the runner
> exits 3 and tells you the documentation is out of date.

**Related issues:** [#131](https://github.com/Guepard-Corp/gfs/issues/131), [#132](https://github.com/Guepard-Corp/gfs/issues/132)

## Why this test exists

E1 (#131): a clone is NOT a point-in-time snapshot.

The property under test is reproducibility, not correctness. Reads ARE kept
current by drift detection, which is precisely the problem: the same query
returns different answers over time with no local action at all, so a clone
cannot be used as a stable branch for a repeatable test run.

Declared KNOWN-OPEN. #132 (snapshot mode) is the fix.

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 3 rows: Alice 50, Bob 30, Carol 20
notes(id, body) with 1 row
other(id, v) with 1 row, used only to trigger background checks
```

**Then the source does:**

```sql
INSERT INTO orders VALUES (4,'Dave',40);
```

## What is asserted

- the clone shifted under the reader ($FIRST then $SECOND) with no local action (#131, fixed by #132)

## Running it

```bash
tests/paths/run-all.sh E1          # through the runner
bash tests/paths/E1_torn_across_tables/test.sh   # directly
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
