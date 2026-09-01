# H7: clone --snapshot yields a detached point-in-time clone

**Related issues:** [#132](https://github.com/Guepard-Corp/gfs/issues/132)

## Why this test exists

H7 (#132): `gfs clone --snapshot` = clone + freeze in one step. The result is
born detached: consistent, immune to source writes, and answers offline.

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 3 rows: Alice 50, Bob 30, Carol 20
notes(id, body) with 1 row
other(id, v) with 1 row, used only to trigger background checks
```

**Then the source does:**

```sql
INSERT INTO orders VALUES (4,'Dave',40);
```

## What is asserted

- the CLI announces the snapshot
- the clone is born frozen
- every table was copied up front
- source writes after the snapshot are invisible
- and the snapshot answers with the source stopped

## Running it

```bash
tests/paths/run-all.sh H7          # through the runner
bash tests/paths/H7_snapshot_clone/test.sh   # directly
```

Each test builds its **own** throwaway source and its **own** clone, so it can be
run alone and cannot be affected by any other test.

## Harness helpers used

| helper | what it does |
| --- | --- |
| `P` | runs SQL directly inside the clone container |
| `nudge` | reads an unrelated table so a background drift check can run and commit |
| `src` | runs SQL on the SOURCE database |
| `with_source_down` | evaluates a query with the source stopped, then restarts it |

See [`../lib/common.sh`](../lib/common.sh) for the harness, and
[`../README.md`](../README.md) for the traps that have produced false results here.
