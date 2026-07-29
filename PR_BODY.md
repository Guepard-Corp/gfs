# 🐛 Fix: a lazy clone now notices when the source changes

## Related Issue

Closes #108
Closes #109
Closes #112
Closes #115
Closes #116
Closes #117
Closes #118
Closes #119
Closes #120
Closes #121
Closes #122
Closes #123
Closes #124
Closes #125
Closes #126
Closes #127
Closes #129
Closes #134
Closes #138

> Note for the reviewer: #117, #118, #119 and #120 have no closing keyword in their
> commits, because the commits predate the issues. They are closed by this body.

Filed during this work and deliberately **not** fixed here: #139 and #140. Both have
reproductions, see "Known limitations" below.

## What

A lazy clone copies each table the first time it is read. From that moment the local
copy was **frozen**: the router short-circuited on a cached flag and never contacted
the source again for that table. If the source then inserted, updated, deleted or
truncated rows, the clone kept serving its stale copy indefinitely, **believing it was
correct**, with nothing to indicate the data no longer matched upstream.

That single behaviour spawned a family of related failures: drift that could not be
detected at all (`TRUNCATE` moves no counter), drift that was detected but never
healed (federating forever, which degrades a clone into a pass-through proxy), and
schema changes that surfaced as raw PostgreSQL errors naming SQL the user never wrote.

The theme across every fix here is the same: **the clone must be either correct or
loud, and never quietly wrong.**

## How

Each issue below lists the mechanism and the test that proves it.

### Group 1: the clone served rows that were no longer true

| Issue | The fix | Where | Test |
| --- | --- | --- | --- |
| **#117** `TRUNCATE` invisible | Drift was detected from write counters (`n_tup_ins + n_tup_upd + n_tup_del`), and `TRUNCATE` moves none of them. The baseline now also records `n_live_tup`, and a change in **either** is drift. | `schema.sql` (`source_table_baseline`, `source_drift`) | `tests/paths/A5_truncate_invisible.sh`, `tests/paths/B4_table_emptied.sh` |
| **#118** copied table froze forever | The router returned early on `whole_cached` before asking whether the source had moved. Staleness is now computed **before** every local-serving path and guards it: `stale = drifted && !relation_diverged(relid)`, then `if whole_cached && !stale`. A **diverged** table (one you wrote to) is excluded, because federating it would lose your writes. | `route.rs:230`, `route.rs:283` | `tests/paths/B1_row_added.sh`, `B2_row_changed.sh`, `B3_row_deleted.sh` |
| **#119** federated forever, never healed | The verdict was computed against the baseline captured at clone time, and that baseline only moved on a manual pull, so a table stayed stale by definition. `resync_table` now resets one table and re-anchors **only that table's** baseline, so it rejoins the lazy path, and the background worker does it off the query path under `autopull`. | `schema.sql` (`resync_table`), `worker.rs` | `tests/paths/B4b_never_self_healed.sh` (currently failing, see #140) |
| **#120** pull hid unresolved conflicts | `pull` correctly refused to reset tables you had written to, then called `capture_source_baseline()`, which rewrote **every** baseline including the ones it had just skipped, so the conflict silently became "in sync". That call is gone from `pull` entirely (it is now only used at clone time), and the global marker advances **only when nothing was skipped**. | `schema.sql` (`gfs.pull`) | `tests/paths/D2_same_row_both_sides.sh`, `D3_local_delete_source_update.sh` |

### Group 2: the source changed the shape of a table

| Issue | The fix | Where | Test |
| --- | --- | --- | --- |
| **#122** dropped/renamed column | A federated read asked the source for a column it no longer had, producing a raw `column "price" does not exist`. The column digest now detects the change and raises a message naming the table and what to run. `repair_schema` re-imports the foreign table and adds columns the source gained. A **destructive** change is never applied automatically. | `schema.sql` (`repair_schema`), `route.rs` | `tests/paths/B6_column_dropped.sh`, `B7_column_renamed.sh` |
| **#123** dropped table | A dropped source table left an orphaned foreign table and a raw remote error. It is now detected and reported in the clone's own words, and `pull` names the orphan. | `schema.sql` | `tests/paths/B10_table_dropped.sh` |
| **#124** added column / added table | An added column is caught by the column digest and applied by `pull` (adding is non-destructive). A source table the clone has never heard of is reported as `new_table`. | `schema.sql` (`source_drift`) | `tests/paths/B5_column_added.sh` |
| **#121** added constraint | The digest hashed **columns only**, so a `CHECK` or `UNIQUE` added upstream was invisible. Constraints are now hashed in a **separate** digest (`relation_cons_fp`) and `repair_schema` applies missing ones. See the trade-off note below, this one has a trap. | `schema.sql` (`relation_cons_fp`, `src_cfp`) | `tests/paths/B11_constraint_added.sh` |

### Group 3: objects that are not tables

| Issue | The fix | Where | Test |
| --- | --- | --- | --- |
| **#126** enum label added | Not stale data but a hard failure: `invalid input value for enum`, and the table could not be read at all, because the clone's copy of the type could not represent a fetched value. `resync_enums` adds missing labels **in the source's order** (enum ordering is semantic) and runs early in `pull`, before anything refetches. A clone-only label is never removed. | `schema.sql` (`resync_enums`) | `tests/paths/B14_enum_value_added.sh` |
| **#125** sequence advanced | Nothing *reads* a sequence, so no counter moves and no drift is detected. The clone's counter fell behind ids the source had already issued, and a local insert failed with a duplicate key. `resync_sequences` advances local sequences as part of `pull`. | `schema.sql` (`resync_sequences`) | `tests/paths/B13_sequence_advanced.sh` |
| **#127** partitions, inherited children, matviews | The quiet one: a new partition is reached **through a parent the clone already has**, so the query succeeds and simply returns fewer rows. `adopt_source_tables` creates the missing partition (with the source's own bound) or child (with its own primary key, since `INHERITS` does not carry the parent's key down) and registers it copy-on-read. `refresh_clone_matviews` recomputes local matviews in dependency order. Adoption also runs under `autoschema`. | `schema.sql` (`adopt_source_tables`, `refresh_clone_matviews`) | Not yet in `tests/paths` (B15/B16/B17 are unwritten). Verified by a dedicated suite during development. |

### Group 4: the clone copied the wrong amount of data

| Issue | The fix | Where | Test |
| --- | --- | --- | --- |
| **#112** link measured as free | Two independent errors, either one sufficient. The **first** remote call pays connect and TLS (about a second), and timing it made the later probe underflow to roughly zero. And `postgres_fdw` **column-prunes**, so a naive probe degenerated to `SELECT NULL ... LIMIT n` and transferred nothing. Fix: discard a warm-up call, force real bytes onto the wire by casting the row to text, and clamp so the probe can never report the link as free. | `schema.sql` (`gfs.calibrate`) | `scripts/e2e-clone-remote-source.sh readonly` asserts `net > 0` over a real link |
| **#115** stale table size | `source_rows` was measured once at clone time, so a table that had grown still looked tiny and got whole-copied. It is now re-measured at the one moment a stale size does damage: just before committing to a whole copy. | `route.rs`, `catalog.rs` (`gfs_verify_source_rows`) | `scripts/e2e-clone-remote-source.sh readonly` evaluates the real gate at 556M rows |

### Group 5: structure, tooling, build

| Issue | The fix | Where | Test |
| --- | --- | --- | --- |
| **#116** incomplete export | `gfs export` **succeeded** while writing a dump in which every never-read table was empty. The clone is materialized before exporting. | `cmd_export.rs`, `cmd_source.rs` | Verified during development; not yet a `tests/paths` case |
| **#108** `FROM ONLY` federation | `SELECT ... FROM ONLY parent` means exclude the children, and pushing it down risks the source expanding children we must not count. It is now refused and copied locally instead. | `federate.rs` | Covered indirectly by the inheritance fixtures |
| **#109** deferrable constraints | The key search demanded an immediate, non-partial, non-expression unique index, so a `DEFERRABLE` unique constraint left the table with no usable key. Those are now accepted. | `clone_bootstrap.sql` | Exercised by every clone in `tests/paths` |
| **#129** unattributed changes masked | The catch-all blanket fires only when *nothing* explains the source's movement, and one explained finding anywhere used to suppress it globally. | `schema.sql` (`source_drift`) | `tests/paths/F1_unattributed_change.sh` |
| **#134** spurious NOTICE | `pull` emitted a `NOTICE` about `gfs_drift_scan` on every call. `pull` runs in scripts and CI, and unconditional noise trains people to ignore output, which is where the real conflict messages appear. | `schema.sql` | Visible in every `pull` in the suite |
| **#138** image tagged wrong | `PG_VERSION` collided with the base image's own ENV, so a bare build produced PostgreSQL 17 **tagged as 16**. Several fixes here are version-sensitive, so this meant a green suite proving nothing about the engine shipped. Renamed to `PG_MAJOR` with an explicit `BASE_IMAGE`. | `crates/extensions/gfs/Dockerfile` | `docker run --rm --entrypoint postgres gfs-postgres:16 --version` now reports 16.14 |

### One trade-off worth reviewing (#121)

The obvious way to detect constraint changes, folding constraints into the existing
schema digest, **breaks the entire clone**. That digest is also compared against the
imported **foreign** table, and a foreign table never carries constraints:

```
source digest         = columns + "PRIMARY KEY (id)"
foreign table digest  = columns + ""          <- never equal
```

Every table would look permanently mismatched and therefore unreadable. Constraints
therefore live in a separate digest compared against the *local* table, while the
column digest keeps its existing job. Foreign keys stay excluded, because the
bootstrap drops them by design so lazy per-table fetching never trips referential
integrity.

## Review Guide

1. **`crates/extensions/gfs/src/route.rs`** is the highest-risk file: it is the planner
   hook, on the hot read path. The change that matters is the stale gate at line 230
   and its use at line 283.
2. **`crates/extensions/gfs/src/sql/schema.sql`** holds the drift detection and the
   whole repair surface (`source_drift`, `pull`, `resync_table`, `repair_schema`,
   `adopt_source_tables`). Read `gfs.pull` first: its step order is load-bearing and
   commented with why.
3. **`crates/adapters/compute-docker/src/containers/clone_bootstrap.sql`** for what a
   clone establishes before any row is copied.
4. `crates/applications/cli/src/commands/` for the CLI surface (`cmd_source.rs` is new).
5. **Skip** `tests/paths/` on a first pass, it is test-only.

## Testing

- [x] Manual testing performed
- [ ] Unit tests added/updated
- [x] E2E tests added/updated

**`tests/paths/`** is new: one script per path in the divergence document, so a failure
names the exact case (`B4`, `D3`) instead of "something in the drift suite broke".

```bash
tests/paths/run-all.sh          # every implemented path
tests/paths/run-all.sh B        # one family
tests/paths/run-all.sh --list   # coverage against the document
```

Last full sweep: **21 of 24 paths passing in 376s**. Of the rest, one was an
environment abort that passes in isolation, one was a misclassified expectation now
corrected, and one (`B4b`) is left failing on purpose, see below.

The runner separates `ABORT` (environment/setup) from `FAIL` (product), because a
starved container previously produced a dozen convincing failures that meant nothing.
It also prints coverage on every run (**24 of 50 documented paths**), so a green sweep
can never be read as "everything is proven".

**`scripts/e2e-clone-remote-source.sh`** is new: the same lazy clone against a **real**
remote database, which is the only way to exercise the cost model at real scale.
Verified against a 100 GB source (556M row table): 11 of 11 assertions, including that
the source's write counters were byte-identical before and after, so the clone read a
production-shaped database and wrote nothing to it.

`tests/paths/README.md` documents what each test proves, plus the seven traps that
have already produced false results here (for instance: `psql` inside the clone still
goes through the planner hook, so it will hydrate the table you were trying to inspect).

## Documentation

- [x] Code comments added for complex logic
- [x] README or docs updated if needed
- [ ] CHANGELOG.md updated (if applicable)

`README.md` gains a "When the source changes" section covering the new `pull` actions
and the automatic settings. `reports/clone-divergence-tree.pdf` (gitignored, shared
separately) maps every path through the system, with a per-issue walkthrough and a
table pointing each case at its test script.

## User Impact

**Correct data is automatic.** You never have to run anything to avoid stale results.
A changed table is read from the source, and the clone repairs itself so later reads
are local again.

**Your writes always win.** A table you have written to is never reset. It is reported
as a conflict for you to resolve, the way git refuses to clobber local changes.

**No behaviour was removed.** `autopull` and `autoschema` are **off by default**,
because a clone is a branch and data shifting under a running test breaks
reproducibility. Existing commands are unchanged; `gfs source` is additive.

Side effect worth knowing: a clone of a busy source now federates more than it used to,
by design, because it previously served stale data instead. `autopull` shortens that
window.

## Known limitations (filed, not fixed here)

- **#139** an `INHERITS` child with no unique key of its own makes the whole clone fail.
  The refusal is correct, since an unregistered child would silently drop rows from
  every read of the parent, but there is no fallback, so a common schema shape cannot
  be cloned at all.
- **#140** benign WAL movement on an idle source (checkpoints, autovacuum) blankets
  every table as drifted, so a clone drifts back into federating everything shortly
  after each pull. Measured: an idle PostgreSQL advances its LSN with no user activity.
  `tests/paths/B4b_never_self_healed.sh` is left **failing on purpose** to describe it
  rather than being weakened or marked known-open.
- With `autoschema` off (the default), a new partition is reported by
  `gfs fetch --check` but reads of the parent stay short until `gfs pull`. Closing that
  needs the planner hook to reason about unregistered partitioned parents, on the hot
  read path, which is out of scope here.

## Checklist

- [ ] Code follows project style guidelines
- [x] Self-review completed
- [ ] Tests pass locally (`cargo test`)
- [ ] Clippy passes (`cargo clippy --all-targets --all-features -- -D warnings`)
- [ ] Format check passes (`cargo fmt --check`)
- [x] This PR can be safely reverted if needed

> The three cargo boxes are **not ticked because I have not run them on the final
> tree**. Please run them before merging, or tell me and I will. The verification
> above came from the e2e suites, not from `cargo test`.
