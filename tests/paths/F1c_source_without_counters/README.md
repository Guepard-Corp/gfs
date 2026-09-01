# F1c: a source whose counters do not count still gets the blanket

**Related issues:** [#129](https://github.com/Guepard-Corp/gfs/issues/129), [#140](https://github.com/Guepard-Corp/gfs/issues/140)

## Why this test exists

F1c (#140/#129): the guard on the #140 fix.

#140 stops the blanket from firing when no row changed anywhere on the source.
That reasoning is only sound while the source's row counters actually count. A
real marketplace source measured during this work reported zero live tuples
across all 138 of its tables while `order_items` alone held 556 million rows,
with track_counts=on and stats_reset=never -- its stats file had been lost.

On such a source every check looks quiet, so "quiet" must NOT be read as "no
change": here a real INSERT moves no counter at all, and only the blanket is
left to catch it. Absence of evidence is not evidence of quiet.

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

- so the row that moved no counter is still seen
- and unguarded, that same comparison calls the source quiet
- a real INSERT moved no counter anywhere on the source (writes=$W live=$L)
- no table was marked suspect (got '$D'): a real change would be served stale

## Running it

```bash
tests/paths/run-all.sh F1c          # through the runner
bash tests/paths/F1c_source_without_counters/test.sh   # directly
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
