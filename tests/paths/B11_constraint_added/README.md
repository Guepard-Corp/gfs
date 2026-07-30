# B11: a constraint added on the source is detected and applied

**Related issues:** [#121](https://github.com/Guepard-Corp/gfs/issues/121)

## Why this test exists

B11 (#121): a constraint added upstream. Also guards the regression where
folding constraints into the COLUMN digest made every table mismatch, because
a foreign table can never carry constraints.

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 3 rows: Alice 50, Bob 30, Carol 20
notes(id, body) with 1 row
other(id, v) with 1 row, used only to trigger background checks
```

**Then the source does:**

```sql
ALTER TABLE orders ADD CONSTRAINT total_pos CHECK (total > 0);
```

## What is asserted

- column digests agree on a fresh clone (else everything federates forever)
- the constraint change is detected
- the constraint is applied locally (matched by definition, not name)

## Running it

```bash
tests/paths/run-all.sh B11          # through the runner
bash tests/paths/B11_constraint_added/test.sh   # directly
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
