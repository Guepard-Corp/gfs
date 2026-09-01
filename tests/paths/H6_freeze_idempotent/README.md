# H6: freeze is idempotent; pull/fetch/export behave on a frozen clone

**Related issues:** [#132](https://github.com/Guepard-Corp/gfs/issues/132)

## Why this test exists

H6 (#132): freezing a frozen clone is a no-op that says so (nothing is
re-copied), pull/fetch report "detached", and `gfs export` of a frozen clone
completes with the source STOPPED (materialize must not try to warm).

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 3 rows: Alice 50, Bob 30, Carol 20
notes(id, body) with 1 row
other(id, v) with 1 row, used only to trigger background checks
```

The source is not modified: this test is about what the clone does on its own.

## What is asserted

- and says it is already frozen
- no table was re-copied by the second freeze
- $W0
- pull reports detached
- auto-pull switch explains it has no effect
- fetch reports detached

## Running it

```bash
tests/paths/run-all.sh H6          # through the runner
bash tests/paths/H6_freeze_idempotent/test.sh   # directly
```

Each test builds its **own** throwaway source and its **own** clone, so it can be
run alone and cannot be affected by any other test.

## Harness helpers used

| helper | what it does |
| --- | --- |
| `P` | runs SQL directly inside the clone container |
| `assert_source_untouched` | asserts the source's write counters never moved |
| `clone_now` | clones the source and waits until the clone is queryable |

See [`../lib/common.sh`](../lib/common.sh) for the harness, and
[`../README.md`](../README.md) for the traps that have produced false results here.
