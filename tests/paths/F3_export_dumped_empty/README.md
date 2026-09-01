# F3: exporting a lazy clone produces a COMPLETE dump

**Related issues:** [#116](https://github.com/Guepard-Corp/gfs/issues/116)

## Why this test exists

F3 (#116): a dump reads tables directly, so on a lazy clone every NEVER-READ
table used to dump as EMPTY. The dump SUCCEEDED, which is what made it
dangerous: a well-formed backup missing most of the data.

## The scenario

**The source starts with:**

```
a custom schema, created inline by this test (see the script)
```

The source is not modified: this test is about what the clone does on its own.

## What is asserted

- the table has never been read, so it holds nothing locally
- the never-read table exported all its rows
- no export file was produced at $F

## Running it

```bash
tests/paths/run-all.sh F3          # through the runner
bash tests/paths/F3_export_dumped_empty/test.sh   # directly
```

Each test builds its **own** throwaway source and its **own** clone, so it can be
run alone and cannot be affected by any other test.

## Harness helpers used

| helper | what it does |
| --- | --- |
| `clone_now` | clones the source and waits until the clone is queryable |
| `local_bytes` | physical size of the local heap; never scans, so the planner hook cannot fire |

See [`../lib/common.sh`](../lib/common.sh) for the harness, and
[`../README.md`](../README.md) for the traps that have produced false results here.
