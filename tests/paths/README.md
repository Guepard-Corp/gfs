# `tests/paths` — one test per path in the divergence document

Every branch drawn in `reports/clone-divergence-tree.pdf` claims the clone behaves
a certain way. This folder is where those claims are actually proven. There is one
script per documented case, so a failure names the exact path (`B4`, `D3`) instead
of "something in the drift suite broke".

```
tests/paths/
  lib/common.sh    the shared harness (read this before writing a new path)
  run-all.sh       master runner
  A1_never_read.sh …  one script per case
  README.md        this file
```

## Running

```bash
cargo build --release                      # the tests run the real binary
docker build -t gfs-postgres:16 crates/extensions/gfs   # and the real image

tests/paths/run-all.sh            # every implemented path
tests/paths/run-all.sh B          # just the B family
tests/paths/run-all.sh B4 D3      # specific paths
tests/paths/run-all.sh --list     # coverage against the document, runs nothing

GFS_KEEP=1 bash tests/paths/B1_row_added.sh   # leave containers up to poke at
```

Each path builds its **own** throwaway source and its **own** clone, so tests
cannot contaminate each other and any one can be run alone while iterating. A full
sweep takes roughly 14 minutes for all 50 paths on a laptop.

Environment overrides: `GFS_BIN`, `GFS_IMAGE`, `GFS_SOURCE_IMAGE`,
`GFS_TEST_WORKDIR`, `GFS_KEEP`.

## How to read the result

| status | meaning |
| --- | --- |
| `PASS` / `FAIL` | an assertion about the product |
| `ABORT` (exit 90) | the **environment** failed (container never came up, clone never completed). Never counted as a product defect |
| `known-open now passes` (exit 3) | a path declared `--expect open` had every assertion pass. Either a real fix landed, or the assertion is too weak. Update the document |

The runner always prints coverage. All 50 documented paths now have a script; five
are marked `--expect open`, which means they assert TODAY's behaviour for something
still unfixed rather than staying silent about it.

`ABORT` exists because of experience: one starved container previously produced a
dozen convincing `FAIL` lines that meant nothing. Setup failure and product failure
must never look alike.

## Writing a new path

```bash
#!/usr/bin/env bash
# B1 (#118): the source adds a row after the clone cached the table.
. "$(dirname "$0")/lib/common.sh"
case_begin B1 "source INSERT is visible to the clone"
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null      # cache it
src "INSERT INTO orders VALUES (4,'Dave',40);"
nudge
assert_query_eq "SELECT count(*) FROM orders;" 4 "returns the new count, not the cached 3"
case_end
```

Name the file `<CASE>_<slug>.sh`. The runner discovers it and `--list` counts it
as covering that case.

### Harness reference

| helper | what it does |
| --- | --- |
| `case_begin ID "desc" [--expect open]` | starts the case, allocates a per-case port, sets traps |
| `fixture_simple` | source with `orders` (3 rows), `notes`, `other` |
| `fixture_bulk` | source with `orders` (5000 rows) for cost-model paths |
| `fixture_sql "<DDL>"` | arbitrary source DDL, plus `other` |
| `clone_now ["query=string"]` | clones, waits for readiness, sets a 1s drift check interval |
| `clone_must_fail` | for paths where refusing to clone is correct |
| `src "<sql>"` / `srcq "<sql>"` | write / read on the **source** |
| `q` / `val` | read through the CLI (`val` returns one scalar) |
| `P "<sql>"` | raw psql **inside the clone** (see the trap below) |
| `local_bytes <table>` | physical local size; never scans, so the hook cannot fire |
| `clone_state <table> <column>` | a column of `gfs.clone_source` for that table |
| `nudge` | reads an unrelated table so a background drift check can run and commit |
| `wait_until_cached <table> [s]` | polls until the table is genuinely held whole locally |
| `with_source_down "<sql>"` | evaluates with the source stopped, then always restarts it |
| `assert_eq` `assert_query_eq` `assert_src_eq` `assert_match` `assert_nomatch` `assert_true` `assert_false` | assertions |
| `assert_source_untouched <counter>` | the golden rule: gfs must never write upstream |

### Traps that have already produced false results

These are why the helpers exist. Please do not work around them.

1. **`P` goes through the planner hook.** There is no bypass GUC, so
   `P "SELECT count(*) FROM orders"` is *not* a look at the local copy: it runs the
   router and will hydrate the table, destroying the state under test. Use
   `local_bytes` / `clone_state` to inspect. Selecting from `gfs.*` or `pg_catalog`
   is safe, because only registered relations are intercepted.
2. **`pg_isready` lies.** It reports OK while the server is still reaching a
   consistent state, and the setup DDL that follows is silently lost. Always demand
   a real `SELECT 1` (the fixtures do).
3. **A clone can "succeed" and leave a stub config.** Exit code 0 with a
   `config.toml` holding only `version` and `description`. `clone_now` treats that
   as `ABORT`.
4. **`boolean::text` is `true`/`false`, not `t`/`f`.** Comparing the wrong
   rendering invents failures. Use `assert_true` / `assert_false`, which normalise
   via `tf()`.
5. **`timeout` does not exist on macOS.** Never use it. A command not found still
   exits 0 through a pipe and reads as success.
6. **Whole-own completes in the background.** `pull; query; sleep 3` is a race.
   Use `wait_until_cached`.
7. **Stopping the source must always be paired with restarting it.** Use
   `with_source_down`, or one failing path leaves the container down and every
   later path aborts.

## What each test proves


### A: neither side changed

| script | proves |
| --- | --- |
| `A1_never_read` | a never-read table holds nothing locally but reads correctly |
| `A2_partly_copied` | a partly copied table still answers correctly for everything |
| `A3_fully_copied` | a fully copied table is served locally, even with the source down |
| `A4_verdict_is_a_snapshot` | the unchanged verdict is a snapshot, and refreshes on the next check |
| `A5_truncate_invisible` | source TRUNCATE is detected (write counters alone cannot see it) (#117) |

### B: the source changed

| script | proves |
| --- | --- |
| `B1_row_added` | source INSERT is visible to the clone (#118) |
| `B2_row_changed` | source UPDATE is visible to the clone (#118) |
| `B3_row_deleted` | source DELETE removes the row from the clone's answers (#118) |
| `B4_table_emptied` | source TRUNCATE leaves the clone reporting zero rows (#117) |
| `B4b_never_self_healed` | a drifted table self-heals back onto the local path (#119) |
| `B5_column_added` | source ADD COLUMN is detected and repaired by pull (#124) |
| `B6_column_dropped` | source DROP COLUMN is a conflict, and the local column survives (#122) |
| `B7_column_renamed` | source RENAME COLUMN is detected and treated as a conflict (#122) |
| `B8_column_type_changed` | a column type change is detected and does not crash the clone (#121) |
| `B9_new_table_on_source` | a new standalone table on the source is reported by name (#124) |
| `B10_table_dropped` | a table dropped on the source is reported clearly (#123) |
| `B10b_table_renamed` | a renamed source table is reported as a drop plus a new table **[open]** |
| `B11_constraint_added` | a constraint added on the source is detected and applied (#121) |
| `B12_index_changed` | an index change on the source does not affect correctness |
| `B13_sequence_advanced` | source sequence advancing does not cause a duplicate key on the clone (#125) |
| `B14_enum_value_added` | an enum label added on the source is replicated in the source's order (#126) |
| `B15_partition_added` | a partition added on the source is adopted, and the parent is not reported as new (#127) |
| `B16_inheritance_child_added` | an inheritance child added on the source is adopted under the same parent (#108, #127, #139) |
| `B17_matview_refreshed` | a matview refreshed on the source becomes current after a pull (#127) |
| `B18_trigger_default_changed` | a changed default on the source is not recognised as a schema change (#140) **[open]** |
| `B19_table_grew` | a table that grew is re-measured before the clone commits to copying it (#115) |

### C: you changed the clone

| script | proves |
| --- | --- |
| `C1_local_insert` | a local INSERT persists and never reaches the source |
| `C2_local_update` | a local UPDATE persists and never reaches the source |
| `C3_local_delete` | a locally deleted row is not resurrected by the source |
| `C4_local_truncate` | a locally emptied table is not repopulated from the source |
| `C5_local_schema_change` | a locally added column survives source activity and a pull |
| `C6_write_to_partly_copied` | writing to a partly copied table completes the copy first |
| `C7_write_to_never_read` | writing to a never-read table copies it first |
| `C8_local_ids_consumed` | ids consumed on the clone do not affect the source |

### D: both sides changed (conflicts)

| script | proves |
| --- | --- |
| `D1_disjoint_edits` | disjoint edits are reported as a conflict because tracking is per table (#130) **[open]** |
| `D2_same_row_both_sides` | both sides changed the same row: the local version wins, loudly |
| `D3_local_delete_source_update` | you deleted, the source updated: the row stays deleted |
| `D4_local_update_source_delete` | you updated, the source deleted: your row is kept and reported |
| `D5_both_inserted_same_id` | both sides inserted the same id: the local row wins, loudly |
| `D6_local_empty_source_insert` | you emptied, the source inserted: the table stays empty |
| `D7_both_changed_schema` | both sides changed the schema: local columns are preserved |
| `D8_local_data_source_dropped_column` | your data plus a source column drop: the column is not dropped locally |
| `D9_local_schema_source_data` | your schema plus source data: the local schema survives |

### E: across tables and moments

| script | proves |
| --- | --- |
| `E1_torn_across_tables` | a clone changes under you: the same query gives different answers over time (#131, #132) **[open]** |
| `E2_torn_within_table` | one table can reflect two different moments (#131) **[open]** |

### F: detection limits

| script | proves |
| --- | --- |
| `F1_unattributed_change` | a fresh clone reports no drift, and the verdict is honest (#129) |
| `F2_verdict_is_a_snapshot` | the unchanged verdict has a window, and it closes on the next check |
| `F3_export_dumped_empty` | exporting a lazy clone produces a COMPLETE dump (#116) |

### G: availability

| script | proves |
| --- | --- |
| `G1_source_unreachable_cached` | a copied table still answers when the source is unreachable |
| `G2_source_unreachable_uncached` | an uncopied table fails loudly offline, it does not fabricate an answer |

## Known limitation this suite exposed

`B4b` currently fails, and the cause is a product limitation rather than a test
bug. Measured:

```
after pull + recopy : whole_cached=true  local_bytes=8192  drifted=false
after one nudge     : drifted=TRUE  with baseline 4/4 and source 4/4 (identical)
fetch --check       : "3 of 3 tables changed on the source"
```

No per-table finding fired, because the baseline matches the source exactly. What
fired is the **unattributed blanket**. The trigger is that an idle PostgreSQL
source advances its own WAL position (checkpoints, autovacuum), measured directly:

```
t1: 0/1586E30
t2: 0/1586E30
t3: 0/1586E68     ← no user activity at all
```

So the LSN moves, nothing can account for it, and the safe response marks every
table suspect. The consequence is that a clone drifts back into federating
everything shortly after each `pull`, which is #119's symptom returning through a
different door. The blanket itself is deliberate and correct in intent; the
problem is that benign WAL movement is indistinguishable from a real unattributable
change.

`B4b` is left **failing on purpose** rather than being weakened or marked
known-open, because it is describing something worth fixing.

## Coverage

All **50** documented cases have a script. `run-all.sh --list` shows the mapping.

Latest full sweep: **49 of 50 passing in 812s, zero aborts.** `B4b` fails on purpose
(see above).

Five paths are marked `--expect open`. They assert what the system does *today* for
something still broken, so the suite describes the gap instead of omitting it:

| | gap |
| --- | --- |
| `B10b_table_renamed` | a rename reads as a drop plus an unrelated new table |
| `B18_trigger_default_changed` | defaults, triggers and functions are in no digest |
| `D1_disjoint_edits` | divergence is per table, so mergeable edits look like conflicts (#130) |
| `E1_torn_across_tables` | a clone is not a stable snapshot: the same query can answer differently later (#131) |
| `E2_torn_within_table` | the same tear inside one table (#131) |

If one of these starts passing, the runner exits 3 and says the document is out of
date rather than quietly going green. That has already earned its keep: it caught
three tests that were passing for the wrong reason. `B18` was matching the table name
in `fetch --check` output, but any DDL moves the WAL and the unattributed blanket then
names every table, so it was measuring the blanket rather than default detection.
`E1` and `E2` passed because drift detection keeps reads current, which masks the very
tear they were meant to show.


## Related suites

| | scope |
| --- | --- |
| `cargo test --test e2e_clone_postgres` | hermetic Rust e2e, throwaway source, macOS only |
| `scripts/e2e-clone-remote-source.sh` | a **real** remote source: cost model at real scale, calibration over a real link. `readonly` mode is safe on production-shaped data; `drift` mode mutates only its own scratch schema |
| `tests/paths/` | one test per documented divergence path (this folder) |
