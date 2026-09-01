# B10: a table dropped on the source is reported clearly

**Related issues:** [#123](https://github.com/Guepard-Corp/gfs/issues/123)

## Why this test exists

B10 (#123): the source drops a whole table, leaving an orphaned foreign table.

## The scenario

**The source starts with:**

```
a custom schema, created inline by this test (see the script)
```

**Then the source does:**

```sql
DROP TABLE goner;
```

## What is asserted

- the drop is reported
- pull mentions the orphaned table

## Running it

```bash
tests/paths/run-all.sh B10          # through the runner
bash tests/paths/B10_table_dropped/test.sh   # directly
```

Each test builds its **own** throwaway source and its **own** clone, so it can be
run alone and cannot be affected by any other test.

## Harness helpers used

| helper | what it does |
| --- | --- |
| `clone_now` | clones the source and waits until the clone is queryable |
| `nudge` | reads an unrelated table so a background drift check can run and commit |
| `q` | reads through the gfs CLI and returns the whole output |
| `src` | runs SQL on the SOURCE database |
| `val` | reads through the gfs CLI and returns one value |

See [`../lib/common.sh`](../lib/common.sh) for the harness, and
[`../README.md`](../README.md) for the traps that have produced false results here.
