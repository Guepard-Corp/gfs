# B15: a partition added on the source is adopted, and the parent is not reported as new

**Related issues:** [#127](https://github.com/Guepard-Corp/gfs/issues/127)

## Why this test exists

B15 (#127): the source adds a PARTITION to a table the clone already has.

This is the QUIET failure. A new partition is reached through a parent the
clone already serves, so the query SUCCEEDS and simply returns fewer rows than
the source holds. Nothing errors, nothing warns.

Also guards the false positive the fix exposed: gfs.source_map is built from
clone_source JOIN pg_foreign_table, so a partitioned PARENT (relkind='p', never
registered because it stores no rows) can never appear in it. Testing "new" as
"absent from source_map" reported the parent as a brand new table forever, and
because new_table COUNTS AS ATTRIBUTION that phantom switched off the
unattributed blanket entirely, masking real drift.

## The scenario

**The source starts with:**

```
a custom schema, created inline by this test (see the script)
```

**Then the source does:**

```sql
CREATE TABLE ev_2026 PARTITION OF ev FOR VALUES FROM ('2026-01-01') TO ('2027-01-01');
     INSERT INTO ev VALUES (4,'2026-02-01','new'),(5,'2026-05-01','new2');
INSERT INTO ev VALUES (6,'2026-08-01','later');
```

## What is asserted

- the partitioned table reads through the parent
- both leaf partitions are registered for copy-on-read
- the partitioned parent is deliberately NOT registered (it stores no rows)
- the new partition is named by fetch --check
- after pull the clone has every row, including the new partition
- a LATER write into the adopted partition is picked up too
- PHANTOM: the partitioned parent public.ev is reported as a new table
- the partitioned parent is NOT reported as new

## Running it

```bash
tests/paths/run-all.sh B15          # through the runner
bash tests/paths/B15_partition_added/test.sh   # directly
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

See [`../lib/common.sh`](../lib/common.sh) for the harness, and
[`../README.md`](../README.md) for the traps that have produced false results here.
