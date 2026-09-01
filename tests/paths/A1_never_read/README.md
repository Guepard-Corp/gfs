# A1: a never-read table holds nothing locally but reads correctly

**Related issues:** [#133](https://github.com/Guepard-Corp/gfs/issues/133)

## Why this test exists

A1: a table never read cannot be stale. It holds NO rows locally, and the very
first read must still return exactly what the source has.

Note the ordering below: every local-state assertion happens BEFORE the first
read. Scanning the table through psql would go through the planner hook and
hydrate it, destroying the state under test (see local_bytes in common.sh).

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 3 rows: Alice 50, Bob 30, Carol 20
notes(id, body) with 1 row
other(id, v) with 1 row, used only to trigger background checks
```

The source is not modified: this test is about what the clone does on its own.

## What is asserted

- the local heap has zero pages: nothing was copied at clone time
- orders is not marked whole-cached
- no partial slice has been pulled either
- the first read returns the source's rows
- remote prints the source host and port
- remote states the fetch-only rule
- the source password is never printed

## Running it

```bash
tests/paths/run-all.sh A1          # through the runner
bash tests/paths/A1_never_read/test.sh   # directly
```

Each test builds its **own** throwaway source and its **own** clone, so it can be
run alone and cannot be affected by any other test.

## Harness helpers used

| helper | what it does |
| --- | --- |
| `clone_now` | clones the source and waits until the clone is queryable |
| `clone_state` | reads a column of gfs.clone_source for one table |
| `local_bytes` | physical size of the local heap; never scans, so the planner hook cannot fire |

See [`../lib/common.sh`](../lib/common.sh) for the harness, and
[`../README.md`](../README.md) for the traps that have produced false results here.
