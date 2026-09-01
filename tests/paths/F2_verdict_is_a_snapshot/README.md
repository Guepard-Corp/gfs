# F2: the unchanged verdict has a window, and it closes on the next check

## Why this test exists

F2: "unchanged" is only true as of the last check. This is a real detection
limit, not a bug: between checks the clone can be confidently out of date.

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 3 rows: Alice 50, Bob 30, Carol 20
notes(id, body) with 1 row
other(id, v) with 1 row, used only to trigger background checks
```

**Then the source does:**

```sql
INSERT INTO orders VALUES (4,'Dave',40);
```

## What is asserted

- once a check runs, the verdict is corrected

## Running it

```bash
tests/paths/run-all.sh F2          # through the runner
bash tests/paths/F2_verdict_is_a_snapshot/test.sh   # directly
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
