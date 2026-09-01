# B10b: a renamed source table is reported as a drop plus a new table

> **This test is expected to FAIL.** It is marked `--expect open`, which means it
> asserts what the system does *today* for something that is still unfixed. The
> suite counts it as passing. If it ever starts passing on its own, the runner
> exits 3 and tells you the documentation is out of date.

## Why this test exists

B10b: the source RENAMES a table. Declared KNOWN-OPEN: there is no rename
detection, so it reads as a drop plus an unrelated new table. This documents
today's behaviour so a future fix has something to flip.

## The scenario

**The source starts with:**

```
a custom schema, created inline by this test (see the script)
```

**Then the source does:**

```sql
ALTER TABLE oldname RENAME TO newname;
```

## What is asserted

- the new name is at least reported as a new table
- the rename is not recognised as such (reads as drop + unrelated new table)

## Running it

```bash
tests/paths/run-all.sh B10b          # through the runner
bash tests/paths/B10b_table_renamed/test.sh   # directly
```

Each test builds its **own** throwaway source and its **own** clone, so it can be
run alone and cannot be affected by any other test.

## Harness helpers used

| helper | what it does |
| --- | --- |
| `clone_now` | clones the source and waits until the clone is queryable |
| `nudge` | reads an unrelated table so a background drift check can run and commit |
| `src` | runs SQL on the SOURCE database |
| `val` | reads through the gfs CLI and returns one value |

See [`../lib/common.sh`](../lib/common.sh) for the harness, and
[`../README.md`](../README.md) for the traps that have produced false results here.
