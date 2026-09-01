# G2: an uncopied table fails loudly offline, it does not fabricate an answer

## Why this test exists

G2: with the source unreachable and the table NEVER copied, there is nothing
to answer from.

The FEATURE gap (no offline mode) is real and still open, but the BEHAVIOUR is
correct today: it fails loudly instead of inventing an answer. So this asserts
the current, correct behaviour and is expected to pass. Read the description,
not the status, for the gap.

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 3 rows: Alice 50, Bob 30, Carol 20
notes(id, body) with 1 row
other(id, v) with 1 row, used only to trigger background checks
```

The source is not modified: this test is about what the clone does on its own.

## What is asserted

- the table really has not been copied
- no number is returned when the data is not held and the source is gone
- it reports a failure explicitly

## Running it

```bash
tests/paths/run-all.sh G2          # through the runner
bash tests/paths/G2_source_unreachable_uncached/test.sh   # directly
```

Each test builds its **own** throwaway source and its **own** clone, so it can be
run alone and cannot be affected by any other test.

## Harness helpers used

| helper | what it does |
| --- | --- |
| `clone_now` | clones the source and waits until the clone is queryable |
| `local_bytes` | physical size of the local heap; never scans, so the planner hook cannot fire |
| `with_source_down` | evaluates a query with the source stopped, then restarts it |

See [`../lib/common.sh`](../lib/common.sh) for the harness, and
[`../README.md`](../README.md) for the traps that have produced false results here.
