# H1: freeze ends the tear: every table from one instant, answers stop moving

**Related issues:** [#131](https://github.com/Guepard-Corp/gfs/issues/131), [#132](https://github.com/Guepard-Corp/gfs/issues/132)

## Why this test exists

H1 (#132, closes #131): two tables copied at different moments are TORN -- the
clone holds a combination that never existed at the source. `gfs freeze` must
end that: one snapshot, every table from the same instant, and the answer
stops changing.

The tear is reproduced first (orphan payments > 0), then frozen away. The
drift verdict is deliberately kept stale (check_interval = 1 hour) so the
lazy copies actually go out of step, as they do between background checks.

## The scenario

**The source starts with:**

```
a custom schema, created inline by this test (see the script)
```

**Then the source does:**

```sql
INSERT INTO orders VALUES (4,'Dave',40); INSERT INTO payments VALUES (4,4);
INSERT INTO orders VALUES (5,'Eve',10); INSERT INTO payments VALUES (5,5);
```

## What is asserted

- the tear is real first: an orphan payment for an order the clone lacks (#131)
- freeze ended the tear: no orphan payments
- orders re-copied from the freeze instant
- payments re-copied from the same instant
- the frozen clone holds still while the source moves on
- and stays internally consistent

## Running it

```bash
tests/paths/run-all.sh H1          # through the runner
bash tests/paths/H1_freeze_ends_tearing/test.sh   # directly
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
