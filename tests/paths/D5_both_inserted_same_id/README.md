# D5: both sides inserted the same id: the local row wins, loudly

## Why this test exists

D5: both sides inserted the same id with different data. There is no correct
automatic merge, so the local row must win and the clash must be reported.

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 3 rows: Alice 50, Bob 30, Carol 20
notes(id, body) with 1 row
other(id, v) with 1 row, used only to trigger background checks
```

**Then the source does:**

```sql
INSERT INTO orders VALUES (42,'TheirsRemote',2);
```

## What is asserted

- the local row wins
- the clash is reported
- and the local row is still there

## Running it

```bash
tests/paths/run-all.sh D5          # through the runner
bash tests/paths/D5_both_inserted_same_id/test.sh   # directly
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
