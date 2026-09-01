# H8: freeze clears the torn report: spans-N before, single moment after

**Related issues:** [#131](https://github.com/Guepard-Corp/gfs/issues/131), [#132](https://github.com/Guepard-Corp/gfs/issues/132)

## Why this test exists

H8 (#131 meets #132): the detection/freeze handshake. H1 proves freeze ends
the TEAR (the orphan JOIN empties); H8 proves the REPORT tracks it: torn
with a span before the freeze, single-moment after -- by construction, not
by arithmetic (frozen short-circuits gfs.clone_moments(), and freeze_run
clears each re-copied table's watermark before warm re-stamps it with the
ONE freeze mark, so the raw table never contradicts the verdict).

Also pins frozen PRECEDENCE in the UX: a frozen clone shows the detached
output, never the torn/spans line, and status shows no Moments row.

Reaching "torn" needs a real second copy event, which needs a `gfs pull`
first: a table the drift guard has flagged FEDERATES, and a federated read
copies -- and therefore stamps -- nothing. See E1b for the same sequence and
why it is the honest one. `other` is never touched by the source, so it holds
the moment-A stamp that the moment-B copy is torn against.

## The scenario

**The source starts with:**

```
a custom schema, created inline by this test (see the script)
```

**Then the source does:**

```sql
INSERT INTO orders VALUES (4,'Dave',40); INSERT INTO payments VALUES (4,4);
```

## What is asserted

- torn before the freeze
- gfs fetch shows the span before the freeze
- the verdict switches to frozen
- exactly one moment
- every re-copied table carries the one freeze mark
- the torn line never renders on a frozen clone
- the detached output takes precedence
- status shows the frozen branch
- and no Moments row

## Running it

```bash
tests/paths/run-all.sh H8          # through the runner
bash tests/paths/H8_freeze_clears_torn_report/test.sh   # directly
```

Each test builds its **own** throwaway source and its **own** clone, so it can be
run alone and cannot be affected by any other test.

## Harness helpers used

| helper | what it does |
| --- | --- |
| `P` | runs SQL directly inside the clone container |
| `clone_now` | clones the source and waits until the clone is queryable |
| `src` | runs SQL on the SOURCE database |
| `val` | reads through the gfs CLI and returns one value |
| `wait_until_cached` | polls until the table is genuinely held whole locally |

See [`../lib/common.sh`](../lib/common.sh) for the harness, and
[`../README.md`](../README.md) for the traps that have produced false results here.
