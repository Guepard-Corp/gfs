# gfs — benchmark & validation

There is **one benchmark** for the lazy clone (copy-on-read of an external
PostgreSQL) and its cost-based router: the **benchmark explorer**
(`examples/benchmark-explorer`). It is defined once and driven two ways — a **UI**
and a **headless** runner — so the interactive demo and the CI check exercise the
exact same scenarios. A separate **validation suite** (scale / chaos / write-safety)
covers the harder guarantees.

## The benchmark — `examples/benchmark-explorer`

One set of scenarios over a real multi-table workload (**TPC-H**), on a persistent
source (a named Docker volume per scale factor — the slow generate+load happens
once; only the lazy clone is rebuilt). Both front-ends import the same definitions
(`server/src/bench.ts`): the scenario SQL, clone provisioning, pinned cost weights,
and route detection live in one place.

```bash
cd examples/benchmark-explorer

SF=1 ./scripts/run.sh        # UI:       http://localhost:8789  (click "Clone the source")
SF=1 ./scripts/bench.sh      # headless: asserts + writes benchmark.results.tsv, exit 0/1
```

### Router decision paths it exercises

- **P1 range-hydrate** — a query bounding the integer key (`l_orderkey BETWEEN`)
  fetches the missing key *range*, then serves local (elision on re-ask).
- **P2 partial-selective** — a selective non-key predicate (`l_quantity = v`) on a
  table *too big to own whole* fetches only the matching *slice* (capped,
  self-validating): 1st touch federates, 2nd fetches the slice, 3rd serves local.
- **P3 federate** — joins / aggregates with no key bound (Q1/Q3/Q5/Q10) are pushed
  to the source via `postgres_fdw` (computed remotely, nothing materialized).
- **P5 temporal** — a `DATE`-keyed window (`o_orderdate BETWEEN`) fetches the time
  range, then serves local; a too-wide window federates (capped).
- **convergence** — a federated join becomes **fully local** (zero `Foreign Scan`)
  after `gfs.warm` materializes its tables — the clone converging to a
  self-sufficient copy (the transitive-FK-warming case in the planner-hook model).

Every shot asserts **`clone == source`** (correctness) and the **expected route**.

### Headless output

The verdict is `N passed, M failed`. The objective block reports `source_ops`
(clone→source contacts), `rows_pulled`, and `local_pct`, persists them to
`examples/benchmark-explorer/benchmark.results.tsv` (append-only, one row per
`LABEL`), and flags **IMPROVED / REGRESSED** against the previous run — the
regression guard. Scale up with `SF=10` (~16 GB) / `SF=50` (~100 GB).

### Prerequisites (one-time)

- **Docker**
- **DuckDB** on `PATH` (`duckdb`) — generates TPC-H, no `dbgen` build
- **pnpm** (the explorer is a small Node app)
- the **gfs CLI** at `target/debug/gfs` — `cargo build -p gfs-cli`
- the **`gfs-postgres:16`** image — `docker build -t gfs-postgres:16 crates/extensions/gfs`
  (slow, multi-stage; only after extension changes). `scripts/run.sh` /
  `scripts/bench.sh` build it on first run if absent.

---

## Validation suite (separate from the benchmark)

These prove harder guarantees and are **not** the benchmark — run them when
changing the relevant behavior. They live in `crates/extensions/gfs/`.

| Script | Proves | Data |
|---|---|---|
| `tpch_validate.sh` | **scale** — P1/P2/P3 over real TPC-H joins at SF1…SF50+, on a persistent source | ~1 GB … ~100 GB+ |
| `chaos_test.sh` | **graceful degradation** — source down during copy-on-read errors (never a wrong/partial result) + the aggregate load N clones put on one source | small |
| `write_safety_test.sh` | **write-safety** — a write whose scan would federate is whole-hydrated locally, leaving the source byte-for-byte untouched | reuses TPC-H SF1 |

```bash
SF=1 ./crates/extensions/gfs/tpch_validate.sh      # scale validation
CLONES=3 ./crates/extensions/gfs/chaos_test.sh     # fault injection
./crates/extensions/gfs/write_safety_test.sh       # write guard (after an SF1 source exists)
```

## Notes / gotchas baked into the scripts

- **Don't calibrate at scale.** `gfs.calibrate()` rewrites `net` to measured
  seconds/byte, which makes big tables *whole-ownable* and collapses the P2 partial
  niche. The benchmark and `tpch_validate.sh` pin fixed weights and do **not**
  calibrate.
- **Ceiling scales with the data.** The whole-own ceiling is set to `0.1 ×`
  lineitem's whole-own cost, so the big facts stay not-ownable (P2/P3) while a 5%
  slice still fits — a *fixed* ceiling doesn't generalize across scale factors.
- **Federation pushdown needs `use_remote_estimate`.** The foreign server is
  created with `use_remote_estimate 'true'` + `fetch_size '10000'` so joins/
  aggregates are pushed to the source; without it `postgres_fdw` fetches base rows
  over a cursor and joins locally (catastrophic at scale).
