# B3: source DELETE removes the row from the clone's answers

**Related issues:** [#118](https://github.com/Guepard-Corp/gfs/issues/118)

## Why this test exists

B3 (#118): a deleted row must stop being returned. This is the case where
serving a stale local copy returns rows that exist nowhere upstream.

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 3 rows: Alice 50, Bob 30, Carol 20
notes(id, body) with 1 row
other(id, v) with 1 row, used only to trigger background checks
```

**Then the source does:**

```sql
DELETE FROM orders WHERE id=3;
```

## What is asserted

- the deleted row is gone
- the total reflects the delete

## Running it

```bash
tests/paths/run-all.sh B3          # through the runner
bash tests/paths/B3_row_deleted/test.sh   # directly
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
