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
sweep takes roughly 6 minutes for 24 paths on a laptop.

Environment overrides: `GFS_BIN`, `GFS_IMAGE`, `GFS_SOURCE_IMAGE`,
`GFS_TEST_WORKDIR`, `GFS_KEEP`.

## How to read the result

| status | meaning |
| --- | --- |
| `PASS` / `FAIL` | an assertion about the product |
| `ABORT` (exit 90) | the **environment** failed (container never came up, clone never completed). Never counted as a product defect |
| `known-open now passes` (exit 3) | a path declared `--expect open` had every assertion pass. Either a real fix landed, or the assertion is too weak. Update the document |

The runner always prints coverage (`24 of 50 documented paths have a script`) so a
green run can never be mistaken for "everything is proven".

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

### A — neither side changed

| | proves |
| --- | --- |
| `A1_never_read` | a never-read table holds **zero pages** locally, is not marked cached, and still reads correctly on first access. The lazy-clone premise |
| `A3_fully_copied` | once copied, reads are served locally: proven by answering with the source **stopped** |
| `A5_truncate_invisible` | `TRUNCATE` moves **no** write counter (the test asserts the counters really are unchanged), so only `n_live_tup` reveals it. Guards #117 |

### B — the source changed

| | proves |
| --- | --- |
| `B1_row_added` | a source `INSERT` is visible, not the cached count (#118) |
| `B2_row_changed` | a source `UPDATE` is visible (#118) |
| `B3_row_deleted` | a deleted row stops being returned. Serving stale here returns rows that exist nowhere upstream (#118) |
| `B4_table_emptied` | `TRUNCATE` leaves the clone reporting zero (#117) |
| `B4b_never_self_healed` | after drifting, a table returns to the **local** path instead of federating forever (#119). See "Known limitation" below |
| `B5_column_added` | an added column is detected and repaired by `pull` (#124) |
| `B6_column_dropped` | a drop is **destructive**: reported as a conflict, and the local column survives (#122) |
| `B7_column_renamed` | a rename is detected and treated as a conflict (#122) |
| `B10_table_dropped` | a dropped table is reported in gfs's own words, not a raw remote error (#123) |
| `B11_constraint_added` | a constraint added upstream is detected and applied (#121). Also guards the regression where folding constraints into the **column** digest made every table mismatch, because a foreign table carries no constraints |
| `B13_sequence_advanced` | a local insert does not collide with ids the source already issued (#125) |
| `B14_enum_value_added` | a new label is replicated **in the source's order**, and the table becomes readable again (#126) |

### C — you changed the clone

| | proves |
| --- | --- |
| `C1_local_insert` | a local insert persists, the source does **not** have it, and the source's write counters never move (the golden rule) |
| `C3_local_delete` | a locally deleted row is not resurrected by federating |
| `C7_write_to_never_read` | writing to a never-read table copies it first, so the write lands on complete data |

### D — both sides changed (conflicts)

| | proves |
| --- | --- |
| `D2_same_row_both_sides` | the local value wins and a conflict is reported |
| `D3_local_delete_source_update` | a row you deleted is not revived by a source update |
| `D7_both_changed_schema` | a locally added column survives a schema repair |

### F, G — detection limits and availability

| | proves |
| --- | --- |
| `F1_unattributed_change` | a fresh clone reports zero findings, and a real change is attributed to a table (#129) |
| `G1_source_unreachable_cached` | a copied table still answers with the source unreachable |
| `G2_source_unreachable_uncached` | an uncopied table **fails loudly** offline rather than fabricating an answer. The missing offline mode is still a gap, but the behaviour is correct |

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

24 of the 50 documented cases have a script. `run-all.sh --list` shows exactly
which. Not yet written:

```
A2 A4 B8 B9 B10b B12 B15 B16 B17 B18 B19
C2 C4 C5 C6 C8 D1 D4 D5 D6 D8 D9 E1 E2 F2 F3
```

Some of those are not pass/fail testable as the system stands (`E1`/`E2` are the
point-in-time gap, `D1` needs row-level divergence tracking, `B12` is speed only).
Those should get a script that documents the current behaviour with
`--expect open`, not silence.

## Related suites

| | scope |
| --- | --- |
| `cargo test --test e2e_clone_postgres` | hermetic Rust e2e, throwaway source, macOS only |
| `scripts/e2e-clone-remote-source.sh` | a **real** remote source: cost model at real scale, calibration over a real link. `readonly` mode is safe on production-shaped data; `drift` mode mutates only its own scratch schema |
| `tests/paths/` | one test per documented divergence path (this folder) |
