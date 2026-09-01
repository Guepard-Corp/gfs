# H5: freeze refuses above the copy budget without copying anything; --force overrides

**Related issues:** [#132](https://github.com/Guepard-Corp/gfs/issues/132)

## Why this test exists

H5 (#132): freezing copies EVERYTHING, so it must refuse a copy bigger than
the byte budget BEFORE moving any data (the honest-limitation guard: a
556M-row source is real, and copying it by accident would be catastrophic).
--force overrides deliberately.

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 5000 rows, big enough that a whole copy
is visible in the clone's own statistics
other(id, v) with 1 row
```

The source is not modified: this test is about what the clone does on its own.

## What is asserted

- the refusal names the budget
- and states the honest limitation
- NOTHING was copied: the never-read table still has zero local bytes
- the clone is not marked frozen
- $W0
- --force takes the snapshot regardless
- the forced snapshot answers offline

## Running it

```bash
tests/paths/run-all.sh H5          # through the runner
bash tests/paths/H5_freeze_guard_size/test.sh   # directly
```

Each test builds its **own** throwaway source and its **own** clone, so it can be
run alone and cannot be affected by any other test.

## Harness helpers used

| helper | what it does |
| --- | --- |
| `P` | runs SQL directly inside the clone container |
| `assert_source_untouched` | asserts the source's write counters never moved |
| `clone_now` | clones the source and waits until the clone is queryable |
| `local_bytes` | physical size of the local heap; never scans, so the planner hook cannot fire |
| `with_source_down` | evaluates a query with the source stopped, then restarts it |

See [`../lib/common.sh`](../lib/common.sh) for the harness, and
[`../README.md`](../README.md) for the traps that have produced false results here.
