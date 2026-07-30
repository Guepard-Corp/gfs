# B6: source DROP COLUMN is a conflict, and the local column survives

**Related issues:** [#122](https://github.com/Guepard-Corp/gfs/issues/122)

## Why this test exists

B6 (#122): dropping a column is DESTRUCTIVE, so it must never be applied
automatically. The local column has to survive.

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 3 rows: Alice 50, Bob 30, Carol 20
notes(id, body) with 1 row
other(id, v) with 1 row, used only to trigger background checks
```

**Then the source does:**

```sql
ALTER TABLE orders DROP COLUMN total;
```

## What is asserted

- clear gfs error
- no raw remote error leaked through
- pull reports a conflict
- the local column was NOT dropped

## Running it

```bash
tests/paths/run-all.sh B6          # through the runner
bash tests/paths/B6_column_dropped/test.sh   # directly
```

Each test builds its **own** throwaway source and its **own** clone, so it can be
run alone and cannot be affected by any other test.

## Harness helpers used

| helper | what it does |
| --- | --- |
| `P` | runs SQL directly inside the clone container |
| `clone_now` | clones the source and waits until the clone is queryable |
| `nudge` | reads an unrelated table so a background drift check can run and commit |
| `q` | reads through the gfs CLI and returns the whole output |
| `src` | runs SQL on the SOURCE database |
| `val` | reads through the gfs CLI and returns one value |

See [`../lib/common.sh`](../lib/common.sh) for the harness, and
[`../README.md`](../README.md) for the traps that have produced false results here.
