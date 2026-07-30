# F1: a fresh clone reports no drift, and the verdict is honest

**Related issues:** [#129](https://github.com/Guepard-Corp/gfs/issues/129)

## Why this test exists

F1 (#129): a change nothing can account for must NOT be hidden just because
some other table has an explainable change. Attribution is load-bearing.

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

- the source changed but no table is marked drifted (got '$D')

## Running it

```bash
tests/paths/run-all.sh F1          # through the runner
bash tests/paths/F1_unattributed_change/test.sh   # directly
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

See [`../lib/common.sh`](../lib/common.sh) for the harness, and
[`../README.md`](../README.md) for the traps that have produced false results here.
