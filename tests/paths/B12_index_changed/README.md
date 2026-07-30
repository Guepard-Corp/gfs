# B12: an index change on the source does not affect correctness

## Why this test exists

B12: an index added or dropped upstream. This is a SPEED-only difference: it
must never change the answers, and must never be treated as a conflict.

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 3 rows: Alice 50, Bob 30, Carol 20
notes(id, body) with 1 row
other(id, v) with 1 row, used only to trigger background checks
```

**Then the source does:**

```sql
CREATE INDEX idx_orders_customer ON orders(customer);
```

## What is asserted

- answers are unchanged by an index
- an index change is not reported as a conflict

## Running it

```bash
tests/paths/run-all.sh B12          # through the runner
bash tests/paths/B12_index_changed/test.sh   # directly
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
