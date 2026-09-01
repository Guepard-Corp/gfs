# D1: disjoint edits are reported as a conflict, by name, and never silently merged

**Related issues:** [#120](https://github.com/Guepard-Corp/gfs/issues/120), [#130](https://github.com/Guepard-Corp/gfs/issues/130)

## Why this test exists

D1 (#130): you and the source edited DIFFERENT rows. In principle mergeable,
but divergence is tracked per TABLE (gfs.clone_source.has_local_writes), not
per row, so GFS cannot prove the two edits do not overlap.

It does not pretend otherwise. The bar this path holds it to is AWARENESS, not
merging: your row survives, both sync verbs name the table as a conflict and
say WHY, `gfs pull` refuses to touch it rather than silently picking a side,
and `gfs status` keeps counting it afterwards. Row-level provenance (the basis
for a real three-way merge) is a separate project -- see #130 and RFC 007.

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 3 rows: Alice 50, Bob 30, Carol 20
notes(id, body) with 1 row
other(id, v) with 1 row, used only to trigger background checks
```

**Then the source does:**

```sql
UPDATE orders SET total=222 WHERE id=3;
```

## What is asserted

- your row is intact (correctness holds)
- gfs fetch reports the divergence as a conflict
- gfs fetch names the table it cannot resolve on its own
- gfs fetch says why: local writes AND a source change
- gfs pull reports the conflict too
- gfs pull leaves the diverged table alone
- gfs pull points at the only way to resolve it
- your row is still yours after the pull
- gfs status still counts the diverged table after the pull skipped it

## Running it

```bash
tests/paths/run-all.sh D1          # through the runner
bash tests/paths/D1_disjoint_edits/test.sh   # directly
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
