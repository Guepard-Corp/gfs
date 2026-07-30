# D1: disjoint edits are reported as a conflict because tracking is per table

> **This test is expected to FAIL.** It is marked `--expect open`, which means it
> asserts what the system does *today* for something that is still unfixed. The
> suite counts it as passing. If it ever starts passing on its own, the runner
> exits 3 and tells you the documentation is out of date.

**Related issues:** [#130](https://github.com/Guepard-Corp/gfs/issues/130)

## Why this test exists

D1 (#130): you and the source edited DIFFERENT rows. In principle mergeable,
but divergence is tracked per TABLE, so this is indistinguishable from a real
conflict. Declared KNOWN-OPEN. Correctness is fine; the limitation is that a
mergeable case is treated as a conflict.

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 3 rows: Alice 50, Bob 30, Carol 20
notes(id, body) with 1 row
other(id, v) with 1 row, used only to trigger background checks
```

**Then the source does:**

```sql
UPDATE orders SET total=222 WHERE id=3;
```

## What is asserted

- your row is intact (correctness holds)
- the disjoint edits were merged rather than treated as a conflict

## Running it

```bash
tests/paths/run-all.sh D1          # through the runner
bash tests/paths/D1_disjoint_edits/test.sh   # directly
```

Each test builds its **own** throwaway source and its **own** clone, so it can be
run alone and cannot be affected by any other test.

## Harness helpers used

| helper | what it does |
| --- | --- |
| `clone_now` | clones the source and waits until the clone is queryable |
| `nudge` | reads an unrelated table so a background drift check can run and commit |
| `q` | reads through the gfs CLI and returns the whole output |
| `src` | runs SQL on the SOURCE database |
| `val` | reads through the gfs CLI and returns one value |

See [`../lib/common.sh`](../lib/common.sh) for the harness, and
[`../README.md`](../README.md) for the traps that have produced false results here.
