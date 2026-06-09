# gfs — lazy copy-on-read clone of an external PostgreSQL, Rust/pgrx

Clones a remote PostgreSQL **copy-on-read**: an empty local database that fetches
data from the source only when a query touches it, so a multi-TB source can be
"cloned" instantly and the clone stays **partial** (it never has to pull what the
app doesn't read). The app cannot tell the clone from an ordinary database — "a
replica, but lazy".

The source is reachable **only over SQL** (a `postgres_fdw` foreign table), so this
is a *logical* clone, not a page-level one. Each clone table is a **real local heap
table** (`relkind='r'`, with the source's indexes, ownership, and writes) **plus** a
foreign table `gfs_remote_*.T`.

## How it works — a `planner_hook`, not a TAM

> An earlier prototype used a Table Access Method (storage layer). It was **removed**:
> the storage layer is blind to a query's predicate, so a seq scan fetched the
> *whole* table — defeating multi-TB laziness. The hook is the right seam: it sees
> the predicate **before** execution. (The old C/PGXS TAM is kept under `c-ref/` for
> reference only.)

A `planner_hook` (`_PG_init` installs it) inspects every query's cold plan and
routes each scan on a registered clone table — a **cost/energy-based** decision, not
a fixed rule:

- **HYDRATE (range)** — a query bounding the table's integer range key
  (`id BETWEEN …`) fetches the missing key **range** into the local table, records it
  in `gfs.cached` (coalesced), then runs local. Re-asking a covered range hits **no
  source** (elision).
- **PARTIAL (selective)** — a selective non-key predicate (`col = v`) on a table
  **too big to own whole** fetches **only the matching slice** with a hard cap, keyed
  in `gfs.cached_predicate` so a repeat serves local. The capped pull self-validates
  selectivity against reality (overflow → federate), so a mis-estimate can never drag
  most of the table over.
- **FEDERATE (join/agg)** — a join/aggregate with no key bound has its clone RTEs
  **rewritten to the foreign tables** (recursively, incl. subqueries/CTEs), so
  `postgres_fdw` **pushes the whole query to the source** and returns the result —
  nothing is materialized locally.
- **OWNED** — once a table is fully materialized (`gfs.warm`, or as ranges fill in),
  it is served locally even for federate-class queries — no source contact.

The choice is driven by an energy model with **whole-own ranked above partial**: a
table affordable to own is owned (1 contact, then all-local); too-big tables get
partial slices or federation. Weights (`gfs.cost`) are **measured** from the source
link by `gfs.calibrate()`; capacity/horizon/partial knobs are policy. A per-clone
**token bucket** (`gfs.budget` / `gfs_throttle`) rate-limits source contact so 100s
of clones can't overwhelm prod (back-pressure, never a wrong result).

Correctness invariant: a scan is served **local only when its rows are provably
present** (covered range, cached predicate, or `whole_cached`); otherwise it
hydrates or federates — never a partial local result.

## Build / test

Built with **cargo-pgrx** (excluded from the parent Cargo workspace):

```bash
cd crates/extensions/gfs
cargo build --no-default-features --features pg16            # typecheck / compile
cargo pgrx install --pg-config "$(which pg_config)"          # build + install into a local PG
docker build -t gfs-postgres:16 .                            # package into an image (slow, multi-stage)
```

The hook-only library loads via `session_preload_libraries='gfs'` (set on the clone
database by `clone_bootstrap.sql`) — `CREATE EXTENSION` alone does not run `_PG_init`.

## Catalog + API

| Object | Role |
|---|---|
| `gfs.clone_source` | per clone table: `source_ref`, `key_col`, `chunk_kind`, `whole_cached`, cost stats (`source_rows`, `row_bytes`, `access_count`, `partial_rows`, `no_partial`) |
| `gfs.cached` | hydrated key ranges (coalesced; range-granular elision) |
| `gfs.cached_predicate` | non-key predicates: `complete` (served local), `overflowed` (not selective → federate) |
| `gfs.cost` | router weights: `net`/`source`/`negligible` (measured), `ceiling`/`horizon`/`prod_load`/`partial_max_frac`/`promote_frac`/`max_partial_preds` (policy) |
| `gfs.budget` | per-clone source-contact rate limit (token bucket) |
| `gfs.clone_stats` / `gfs.clones` | copy-on-read observability (view) |

Functions: `gfs.register_clone(local, source_ref, key_col)` /
`gfs.unregister_clone(local)` / `gfs.warm(local)` (force-materialize + own) /
`gfs.calibrate(sample)` (measure the link → cost weights) / `gfs.note_range` /
`gfs.take_token`. GFS drives these from `clone_bootstrap.sql` (which also imports the
foreign tables, drops FKs so a child can be fetched before its parent, and excludes
generated/dropped columns).

## Files

The extension is split into focused modules under `src/` (the planner hook + crate
root in `lib.rs`, the rest by concern):

| Module | Role |
|---|---|
| `src/lib.rs` | crate root: `_PG_init`, the `planner_hook` (`gfs_planner`/`base_plan`), `mod` declarations, and `extension_sql_file!("sql/schema.sql")` |
| `src/route.rs` | the router — classify each scan, decide local / hydrate / federate |
| `src/keyrange.rs` | extract `[lo,hi]` range-key bounds + const/operator decoding |
| `src/pushdown.rs` | deparse a scan's pushable restriction into a remote `WHERE` |
| `src/federate.rs` | swap clone RTEs to their foreign tables (postgres_fdw pushdown) |
| `src/catalog.rs` | SPI catalog lookups/mutators + the prod-protection throttle |
| `src/hydrate.rs` | the hydration engine (single-statement + parallel dblink fan) |
| `src/model.rs` | descriptors shared across the above |
| `src/sql/schema.sql` | the catalog + API DDL, loaded verbatim via `extension_sql_file!` |

- `Cargo.toml`, `.cargo/config.toml`, `gfs.control` — pgrx crate config.
- `Dockerfile` — package into a `postgres:16` image with the extension.
- **[`BENCHMARKS.md`](BENCHMARKS.md)** — how to run the benchmark
  (`examples/benchmark-explorer`, UI or headless) and the validation suite
  (`tpch_validate.sh` / `chaos_test.sh` / `write_safety_test.sh`).
- `c-ref/` — the original C/PGXS TAM (reference only; superseded by the hook).

## Status / hardening before prod

Validated end-to-end and at scale (TPC-H SF10 / 60M rows): range-hydrate + elision,
cost-computed partial slices, multi-table join/aggregate pushdown (`use_remote_estimate`),
all asserting `clone == source`. Remaining: temporal **time-chunk** hydration
(date/timestamp keys currently federate), trigram/`tsvector` pushability, DELETE
tombstones, a **global** (cross-clone) throttle coordinator, and rewriting the e2e
suite that still references the removed overlay. The planner/SPI path is `unsafe`
FFI; `source_ref`/`key_col`/predicates are admin-set (quote/validate for injection).
