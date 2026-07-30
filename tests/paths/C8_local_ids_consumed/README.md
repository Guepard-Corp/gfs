# C8: ids consumed on the clone do not affect the source

## Why this test exists

C8: you consume id numbers on the clone. Those ids are yours; the source must
not be affected, and the clone must keep issuing usable ids.

## The scenario

**The source starts with:**

```
a custom schema, created inline by this test (see the script)
```

The source is not modified: this test is about what the clone does on its own.

## What is asserted

- both local inserts landed
- the source still has only its own rows
- $W0

## Running it

```bash
tests/paths/run-all.sh C8          # through the runner
bash tests/paths/C8_local_ids_consumed/test.sh   # directly
```

Each test builds its **own** throwaway source and its **own** clone, so it can be
run alone and cannot be affected by any other test.

## Harness helpers used

| helper | what it does |
| --- | --- |
| `assert_source_untouched` | asserts the source's write counters never moved |
| `clone_now` | clones the source and waits until the clone is queryable |
| `q` | reads through the gfs CLI and returns the whole output |
| `val` | reads through the gfs CLI and returns one value |

See [`../lib/common.sh`](../lib/common.sh) for the harness, and
[`../README.md`](../README.md) for the traps that have produced false results here.
