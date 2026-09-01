# E1: a frozen clone holds still: repeated reads agree despite source writes

**Related issues:** [#131](https://github.com/Guepard-Corp/gfs/issues/131), [#132](https://github.com/Guepard-Corp/gfs/issues/132)

## Why this test exists

E1 (#131, fixed by #132): a clone used as a BRANCH must hold still. Before
snapshot mode this was declared KNOWN-OPEN: drift detection kept reads
current, which is precisely the problem -- the same query changed answers
over time with no local action, so no test run was repeatable. `gfs freeze`
is the fix under test: once frozen, repeated reads agree no matter what the
source does.

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

- the clone held still: repeated reads agree, so it behaves as a snapshot (#132)

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
