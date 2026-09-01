# B13: source sequence advancing does not cause a duplicate key on the clone

**Related issues:** [#125](https://github.com/Guepard-Corp/gfs/issues/125)

## Why this test exists

B13 (#125): nothing READS a sequence, so no counter moves and no drift is
detected. The clone's counter falls behind ids the source already issued.

## The scenario

**The source starts with:**

```
a custom schema, created inline by this test (see the script)
```

**Then the source does:**

```sql
INSERT INTO items(v) SELECT 'y'||g FROM generate_series(1,20) g;
```

## What is asserted

- a local insert does not collide with fetched ids

## Running it

```bash
tests/paths/run-all.sh B13          # through the runner
bash tests/paths/B13_sequence_advanced/test.sh   # directly
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
