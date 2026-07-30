# B7: source RENAME COLUMN is detected and treated as a conflict

**Related issues:** [#122](https://github.com/Guepard-Corp/gfs/issues/122)

## Why this test exists

B7 (#122): a rename looks like a drop plus an add, so it is a conflict.

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 3 rows: Alice 50, Bob 30, Carol 20
notes(id, body) with 1 row
other(id, v) with 1 row, used only to trigger background checks
```

**Then the source does:**

```sql
ALTER TABLE orders RENAME COLUMN total TO amount;
```

## What is asserted

- rename detected
- reported as a conflict, not silently applied

## Running it

```bash
tests/paths/run-all.sh B7          # through the runner
bash tests/paths/B7_column_renamed/test.sh   # directly
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
