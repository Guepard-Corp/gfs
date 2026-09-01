# B19: a table that grew is re-measured before the clone commits to copying it

**Related issues:** [#115](https://github.com/Guepard-Corp/gfs/issues/115)

## Why this test exists

B19 (#115): the table grows after clone time. The size used by the cost model
was measured once, at clone time, so a table that was small then could be
whole-copied long after it stopped being small.

The guard runs at ONE point: inside the whole-own gate, just before committing
to a copy. So the table has to be back ON THE LAZY PATH for it to be evaluated.
Straight after the source grows, the table is `drifted` and reads federate,
which never reaches the gate. A pull puts it back on the lazy path, and the
next read is what exercises the guard.

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 3 rows: Alice 50, Bob 30, Carol 20
notes(id, body) with 1 row
other(id, v) with 1 row, used only to trigger background checks
```

**Then the source does:**

```sql
INSERT INTO orders SELECT g,'c'||g,g FROM generate_series(1000,20000) g;
```

## What is asserted

- the answer is correct while federating
- and the answer is still correct afterwards
- source_rows still reads $AFTER (was $BEFORE): the cost model would decide on a stale size

## Running it

```bash
tests/paths/run-all.sh B19          # through the runner
bash tests/paths/B19_table_grew/test.sh   # directly
```

Each test builds its **own** throwaway source and its **own** clone, so it can be
run alone and cannot be affected by any other test.

## Harness helpers used

| helper | what it does |
| --- | --- |
| `clone_now` | clones the source and waits until the clone is queryable |
| `clone_state` | reads a column of gfs.clone_source for one table |
| `nudge` | reads an unrelated table so a background drift check can run and commit |
| `src` | runs SQL on the SOURCE database |
| `val` | reads through the gfs CLI and returns one value |

See [`../lib/common.sh`](../lib/common.sh) for the harness, and
[`../README.md`](../README.md) for the traps that have produced false results here.
