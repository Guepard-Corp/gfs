# B18: a changed default on the source is not recognised as a schema change

> **This test is expected to FAIL.** It is marked `--expect open`, which means it
> asserts what the system does *today* for something that is still unfixed. The
> suite counts it as passing. If it ever starts passing on its own, the runner
> exits 3 and tells you the documentation is out of date.

**Related issues:** [#140](https://github.com/Guepard-Corp/gfs/issues/140)

## Why this test exists

B18: a trigger, default or function changed on the source. Declared
KNOWN-OPEN: none of these are part of any digest, so the change is invisible
AS A SHAPE CHANGE.

Careful with the assertion here. Any DDL moves the source's WAL, and if nothing
accounts for that movement the unattributed blanket marks EVERY table suspect
(see #140). So merely finding the table's name in `fetch --check` proves
nothing: it is the blanket talking, not default detection. The precise question
is whether the change is recognised as a SHAPE change, which is what
schema_drifted records.

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 3 rows: Alice 50, Bob 30, Carol 20
notes(id, body) with 1 row
other(id, v) with 1 row, used only to trigger background checks
```

**Then the source does:**

```sql
ALTER TABLE orders ALTER COLUMN total SET DEFAULT 42;
```

## What is asserted

- the default change is invisible as a shape change (defaults are in no digest)

## Running it

```bash
tests/paths/run-all.sh B18          # through the runner
bash tests/paths/B18_trigger_default_changed/test.sh   # directly
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
