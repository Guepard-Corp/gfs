# C1: a local INSERT persists and never reaches the source

## Why this test exists

C1: you insert on the clone. The row must persist, and the SOURCE must never
see it (the golden rule: gfs never writes upstream).

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 3 rows: Alice 50, Bob 30, Carol 20
notes(id, body) with 1 row
other(id, v) with 1 row, used only to trigger background checks
```

The source is not modified: this test is about what the clone does on its own.

## What is asserted

- the local row is there
- the source does NOT have the row
- $W0

## Running it

```bash
tests/paths/run-all.sh C1          # through the runner
bash tests/paths/C1_local_insert/test.sh   # directly
```

Each test builds its **own** throwaway source and its **own** clone, so it can be
run alone and cannot be affected by any other test.

## Harness helpers used

| helper | what it does |
| --- | --- |
| `assert_source_untouched` | asserts the source's write counters never moved |
| `clone_now` | clones the source and waits until the clone is queryable |
| `q` | reads through the gfs CLI and returns the whole output |

See [`../lib/common.sh`](../lib/common.sh) for the harness, and
[`../README.md`](../README.md) for the traps that have produced false results here.
