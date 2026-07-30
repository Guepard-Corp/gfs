# A5: source TRUNCATE is detected (write counters alone cannot see it)

**Related issues:** [#117](https://github.com/Guepard-Corp/gfs/issues/117)

## Why this test exists

A5 / B4 (#117): TRUNCATE moves no write counter, so it used to look like
"nothing changed". Only n_live_tup reveals it.

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 3 rows: Alice 50, Bob 30, Carol 20
notes(id, body) with 1 row
other(id, v) with 1 row, used only to trigger background checks
```

**Then the source does:**

```sql
TRUNCATE orders;
```

## What is asserted

- the write counters really are unchanged by TRUNCATE (ins/upd/del)
- the clone still reports the table as empty

## Running it

```bash
tests/paths/run-all.sh A5          # through the runner
bash tests/paths/A5_truncate_invisible/test.sh   # directly
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
