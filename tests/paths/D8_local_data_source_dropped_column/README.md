# D8: your data plus a source column drop: the column is not dropped locally

## Why this test exists

D8: you have local data, the source drops a column. Dropping it locally would
destroy your rows' contents, so it must be refused.

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

- reported as a conflict
- the local column, holding your data, is preserved

## Running it

```bash
tests/paths/run-all.sh D8          # through the runner
bash tests/paths/D8_local_data_source_dropped_column/test.sh   # directly
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
