# D4: you updated, the source deleted: your row is kept and reported

## Why this test exists

D4: you updated a row, the source deleted it. Applying the source would
discard an edit you made deliberately.

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 3 rows: Alice 50, Bob 30, Carol 20
notes(id, body) with 1 row
other(id, v) with 1 row, used only to trigger background checks
```

**Then the source does:**

```sql
DELETE FROM orders WHERE id=2;
```

## What is asserted

- your updated row survives
- pull reports it rather than deleting your row
- still there after the pull

## Running it

```bash
tests/paths/run-all.sh D4          # through the runner
bash tests/paths/D4_local_update_source_delete/test.sh   # directly
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
