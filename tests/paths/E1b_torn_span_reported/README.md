# E1b: a torn clone reports the moments it spans; a coherent one reports one

**Related issues:** [#118](https://github.com/Guepard-Corp/gfs/issues/118), [#119](https://github.com/Guepard-Corp/gfs/issues/119), [#131](https://github.com/Guepard-Corp/gfs/issues/131)

## Why this test exists

E1b (#131): tornness is a COMPUTABLE FACT now, not a silent property. Every
copy event stamps gfs.copy_watermark with where the source was when the rows
arrived, and gfs.clone_moments() folds the stamps into a clone-level verdict
with NO source contact.

Two facts are pinned, in order:
* tables copied while the source held still are ONE moment -- benign WAL
movement between the two copies must not count (the F1b lesson applied
to #131: moment identity is the row-activity totals, not the LSN);
* a table copied at a LATER moment makes the clone torn, and both
`gfs fetch` and `gfs status` say so, with the remedy.

Getting the second copy takes a `gfs pull` first, and that is not a detail:
once the source moves, the drift guard flags the affected tables and reads of
them FEDERATE (correct, and the whole point of #118/#119) -- a federated read
copies nothing, so it stamps nothing. `pull` is what puts a flagged table back
on the lazy path, so the next read is a real copy event at the later moment.
`notes` is never touched by the source, so it keeps its moment-A stamp and the
two moments are what the verdict compares.

The orphan-payments JOIN itself (the issue's worked example) lives in H1,
which also proves `gfs freeze` ends it; this is the detection-side complement.

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

- gfs fetch reports the span
- and points at the remedy
- gfs status shows the Moments row
- and marks it torn
- expected moment_count >= 2, got '$MOMENTS'

## Running it

```bash
tests/paths/run-all.sh E1b          # through the runner
bash tests/paths/E1b_torn_span_reported/test.sh   # directly
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
