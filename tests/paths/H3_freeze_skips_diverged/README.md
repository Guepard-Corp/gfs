# H3: freeze keeps a diverged table (and says so), re-copies the rest

**Related issues:** [#132](https://github.com/Guepard-Corp/gfs/issues/132)

## Why this test exists

H3 (#132): a table with LOCAL writes is the user's branch. Freeze must NOT
re-copy it (that would clobber the user's work), must say so, and the frozen
clone still serves it locally -- "the source as of freeze time, plus my
changes". The skip predicate is gfs.relation_diverged_sql, verbatim.

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 3 rows: Alice 50, Bob 30, Carol 20
notes(id, body) with 1 row
other(id, v) with 1 row, used only to trigger background checks
```

**Then the source does:**

```sql
UPDATE orders SET total=99 WHERE id=1; UPDATE notes SET body='upstream' WHERE id=1;
```

## What is asserted

- freeze reports the kept table
- and says WHY it was kept
- the local insert survived the freeze
- the source's change to the kept table was NOT taken
- a non-diverged table WAS re-copied from the freeze instant
- the kept table answers with the source stopped (router serves it locally)

## Running it

```bash
tests/paths/run-all.sh H3          # through the runner
bash tests/paths/H3_freeze_skips_diverged/test.sh   # directly
```

Each test builds its **own** throwaway source and its **own** clone, so it can be
run alone and cannot be affected by any other test.

## Harness helpers used

| helper | what it does |
| --- | --- |
| `P` | runs SQL directly inside the clone container |
| `clone_now` | clones the source and waits until the clone is queryable |
| `q` | reads through the gfs CLI and returns the whole output |
| `src` | runs SQL on the SOURCE database |
| `val` | reads through the gfs CLI and returns one value |
| `with_source_down` | evaluates a query with the source stopped, then restarts it |

See [`../lib/common.sh`](../lib/common.sh) for the harness, and
[`../README.md`](../README.md) for the traps that have produced false results here.
