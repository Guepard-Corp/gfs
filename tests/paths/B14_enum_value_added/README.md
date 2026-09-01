# B14: an enum label added on the source is replicated in the source's order

**Related issues:** [#126](https://github.com/Guepard-Corp/gfs/issues/126)

## Why this test exists

B14 (#126): a label added upstream makes the table UNREADABLE, not merely
stale: the clone's copy of the type cannot represent the fetched value.

## The scenario

**The source starts with:**

```
a custom schema, created inline by this test (see the script)
```

**Then the source does:**

```sql
ALTER TYPE mood ADD VALUE 'excited' AFTER 'happy';
INSERT INTO people VALUES (3,'cy','excited');
```

## What is asserted

- the label is inserted in the SOURCE's order
- the table is readable again

## Running it

```bash
tests/paths/run-all.sh B14          # through the runner
bash tests/paths/B14_enum_value_added/test.sh   # directly
```

Each test builds its **own** throwaway source and its **own** clone, so it can be
run alone and cannot be affected by any other test.

## Harness helpers used

| helper | what it does |
| --- | --- |
| `P` | runs SQL directly inside the clone container |
| `clone_now` | clones the source and waits until the clone is queryable |
| `nudge` | reads an unrelated table so a background drift check can run and commit |
| `src` | runs SQL on the SOURCE database |
| `val` | reads through the gfs CLI and returns one value |

See [`../lib/common.sh`](../lib/common.sh) for the harness, and
[`../README.md`](../README.md) for the traps that have produced false results here.
