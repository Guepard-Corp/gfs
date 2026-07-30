# B8: a column type change is detected and does not crash the clone

**Related issues:** [#121](https://github.com/Guepard-Corp/gfs/issues/121)

## Why this test exists

B8 (#121): the source widens a column's type. The clone must not crash, and
must not silently keep describing the old type.

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 3 rows: Alice 50, Bob 30, Carol 20
notes(id, body) with 1 row
other(id, v) with 1 row, used only to trigger background checks
```

**Then the source does:**

```sql
ALTER TABLE orders ALTER COLUMN total TYPE bigint;
```

## What is asserted

- either a clear gfs message or a correct answer, never a raw remote error
- no crash

## Running it

```bash
tests/paths/run-all.sh B8          # through the runner
bash tests/paths/B8_column_type_changed/test.sh   # directly
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
