# D2: both sides changed the same row: the local version wins, loudly

## Why this test exists

D2: you changed a row, the source changed the same row. There is no correct
automatic answer, so the user's version must win and it must be reported.

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 3 rows: Alice 50, Bob 30, Carol 20
notes(id, body) with 1 row
other(id, v) with 1 row, used only to trigger background checks
```

**Then the source does:**

```sql
UPDATE orders SET total=222 WHERE id=1;
```

## What is asserted

- the local value survives
- pull reports a conflict
- still the local value after pull

## Running it

```bash
tests/paths/run-all.sh D2          # through the runner
bash tests/paths/D2_same_row_both_sides/test.sh   # directly
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
