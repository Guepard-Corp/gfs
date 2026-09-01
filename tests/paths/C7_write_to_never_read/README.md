# C7: writing to a never-read table copies it first

## Why this test exists

C7: writing to a table that was never read must first make the local copy
complete, so the write lands on full data rather than an empty heap.

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 3 rows: Alice 50, Bob 30, Carol 20
notes(id, body) with 1 row
other(id, v) with 1 row, used only to trigger background checks
```

The source is not modified: this test is about what the clone does on its own.

## What is asserted

- nothing copied before the write
- the 3 source rows plus the local one

## Running it

```bash
tests/paths/run-all.sh C7          # through the runner
bash tests/paths/C7_write_to_never_read/test.sh   # directly
```

Each test builds its **own** throwaway source and its **own** clone, so it can be
run alone and cannot be affected by any other test.

## Harness helpers used

| helper | what it does |
| --- | --- |
| `clone_now` | clones the source and waits until the clone is queryable |
| `local_bytes` | physical size of the local heap; never scans, so the planner hook cannot fire |
| `q` | reads through the gfs CLI and returns the whole output |

See [`../lib/common.sh`](../lib/common.sh) for the harness, and
[`../README.md`](../README.md) for the traps that have produced false results here.
