# B17: a matview refreshed on the source becomes current after a pull

**Related issues:** [#127](https://github.com/Guepard-Corp/gfs/issues/127)

## Why this test exists

B17 (#127): the source REFRESHes a materialized view.

A matview on the clone is a LOCAL object computed from the clone's own tables,
and those tables are copy-on-read. So recomputing it locally is what makes it
current: nothing has to be copied from the source's stored matview contents.

## The scenario

**The source starts with:**

```
a custom schema, created inline by this test (see the script)
```

**Then the source does:**

```sql
INSERT INTO prod VALUES (3,30);
REFRESH MATERIALIZED VIEW mv_tot;
```

## What is asserted

- the matview is populated at clone time (a replayed matview arrives empty)
- after pull the matview reflects the refresh
- and its aggregate is recomputed, not just the count
- the matview's BASE table is current too

## Running it

```bash
tests/paths/run-all.sh B17          # through the runner
bash tests/paths/B17_matview_refreshed/test.sh   # directly
```

Each test builds its **own** throwaway source and its **own** clone, so it can be
run alone and cannot be affected by any other test.

## Harness helpers used

| helper | what it does |
| --- | --- |
| `clone_now` | clones the source and waits until the clone is queryable |
| `nudge` | reads an unrelated table so a background drift check can run and commit |
| `src` | runs SQL on the SOURCE database |

See [`../lib/common.sh`](../lib/common.sh) for the harness, and
[`../README.md`](../README.md) for the traps that have produced false results here.
