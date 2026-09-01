# H2: a frozen clone answers everything with the source stopped

**Related issues:** [#132](https://github.com/Guepard-Corp/gfs/issues/132)

## Why this test exists

H2 (#132): a frozen clone is DETACHED. Every table -- including one never read
before the freeze -- answers with the source container STOPPED, and
fetch/pull/status say "detached" instead of erroring. The source stays down
for every assertion: that is the entire promise.

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 3 rows: Alice 50, Bob 30, Carol 20
notes(id, body) with 1 row
other(id, v) with 1 row, used only to trigger background checks
```

The source is not modified: this test is about what the clone does on its own.

## What is asserted

- the clone is marked frozen
- a never-read table was copied by the freeze
- $W0
- orders answers offline
- notes (never read before the freeze) answers offline
- other answers offline
- fetch says the clone is detached, without probing
- fetch --check does not probe a sealed source
- pull says there is no source to sync
- status shows the frozen state

## Running it

```bash
tests/paths/run-all.sh H2          # through the runner
bash tests/paths/H2_frozen_offline/test.sh   # directly
```

Each test builds its **own** throwaway source and its **own** clone, so it can be
run alone and cannot be affected by any other test.

## Harness helpers used

| helper | what it does |
| --- | --- |
| `P` | runs SQL directly inside the clone container |
| `assert_source_untouched` | asserts the source's write counters never moved |
| `clone_now` | clones the source and waits until the clone is queryable |
| `clone_state` | reads a column of gfs.clone_source for one table |
| `val` | reads through the gfs CLI and returns one value |

See [`../lib/common.sh`](../lib/common.sh) for the harness, and
[`../README.md`](../README.md) for the traps that have produced false results here.
