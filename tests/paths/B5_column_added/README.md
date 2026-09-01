# B5: source ADD COLUMN is detected and repaired by pull

**Related issues:** [#124](https://github.com/Guepard-Corp/gfs/issues/124)

## Why this test exists

B5 (#124): an added column is additive, so pull may apply it.

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 3 rows: Alice 50, Bob 30, Carol 20
notes(id, body) with 1 row
other(id, v) with 1 row, used only to trigger background checks
```

**Then the source does:**

```sql
ALTER TABLE orders ADD COLUMN discount int DEFAULT 7;
```

## What is asserted

- a clear gfs error, not a raw remote one
- pull repairs it
- the new column is visible afterwards

## Running it

```bash
tests/paths/run-all.sh B5          # through the runner
bash tests/paths/B5_column_added/test.sh   # directly
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
