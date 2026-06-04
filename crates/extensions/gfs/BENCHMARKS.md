# gfs — benchmarks & validation

Two self-contained scripts validate the lazy clone (copy-on-read of an external
PostgreSQL) and its cost-based router. Run them from the repo root.

| Script | Purpose | Time | Data |
|---|---|---|---|
| [`benchmark.sh`](benchmark.sh) | Deterministic **router** benchmark + non-regression guard | ~2-3 min | tiny, generated each run |
| [`tpch_validate.sh`](tpch_validate.sh) | **Multi-table scale** validation (TPC-H) with a **persistent** source | minutes → ~1 h | ~1 GB … ~100 GB+ |

## Router decision paths these exercise

- **P1 range-hydrate** — a query bounding an integer key (`id BETWEEN`) fetches the
  missing key *range*, then serves local (elision on re-ask).
- **P2 partial-selective** — a selective non-key predicate on a table *too big to
  own whole* fetches only the matching *slice* (capped, self-validating).
- **P3 federate** — joins / aggregates with no key bound are pushed to the source
  via `postgres_fdw` (computed remotely, nothing materialized locally).
- Every check asserts **`clone == source`** (correctness), and reports the route.

## Prerequisites (one-time)

- **Docker**
- **DuckDB** on `PATH` (`duckdb`) — generates TPC-H data, no `dbgen` build needed
- **psql 16** (default: `/opt/homebrew/opt/postgresql@16/bin/psql`)
- the **gfs CLI** at `target/debug/gfs` — build with `cargo build -p gfs-cli`
- the **`gfs-postgres:16`** image — `docker build -t gfs-postgres:16 crates/extensions/gfs`
  (slow, multi-stage; only needed once / after extension changes)

---

## 1. Router benchmark — `benchmark.sh`

Fast, deterministic. Spins up a seeded source + a real gfs clone, fires scenarios,
asserts `clone == source`, shows the route per shot, demonstrates cost
amortization, the prod-protection rate budget, and partial hydration. Persists the
objective metrics to `benchmark.results.tsv` and compares to the previous run
(IMPROVED / REGRESSED / same).

```bash
cd ~/Documents/work/guepard/gfs/gfs
LABEL=my-run ./crates/extensions/gfs/benchmark.sh
```

Knobs (env vars): `LABEL` (row label in the results file), `SHOTS=5` (shots per
scenario), `N_PRODUCTS=50000` / `N_ORDERS=…` (source seed size), `PSQL=…`,
`GFS_IMAGE=…`, `KEEP=1` (don't tear down the containers).

The verdict line is `N passed, M failed`. The objective section compares
`source_ops` / `pull%` / `local%` to the last persisted run — that's the
regression guard.

---

## 2. Multi-table scale validation — `tpch_validate.sh`

Generates TPC-H with DuckDB, loads it (with **primary keys** — the clone bootstrap
only registers tables that have a unique index) into a **persistent** `postgres:16`
source, then `gfs clone`s it and validates P1/P2/P3 over real joins.

```bash
cd ~/Documents/work/guepard/gfs/gfs

SF=1  ./crates/extensions/gfs/tpch_validate.sh      # fast proof (~1 GB, 6M lineitem)
SF=10 ./crates/extensions/gfs/tpch_validate.sh      # ~16 GB, 60M lineitem
SF=50 ./crates/extensions/gfs/tpch_validate.sh      # ~100 GB (long the FIRST time)
```

### The source is PERSISTENT — you don't rebuild it every time

The TPC-H data lives in a **named Docker volume** (`gfs-tpch-vol-sf<SF>`, one per
scale factor). The source container is recreated cheaply each run (with analytical
tuning) **on top of that volume**, so the slow generate+load happens **once**; the
ephemeral, lazy **clone** is what's rebuilt each run (seconds, regardless of source
size — that's the whole point of a lazy clone).

```bash
SF=10 ./crates/extensions/gfs/tpch_validate.sh      # 1st time: builds the source
SF=10 ./crates/extensions/gfs/tpch_validate.sh      # again: "REUSING ... no rebuild"
```

### Knobs

| Variable | Effect |
|---|---|
| `SF=50` | scale factor (≈ GB of raw data; SF50 ≈ ~100 GB in PostgreSQL) |
| `REBUILD_SOURCE=1` | force-regenerate the source for this SF |
| `DROP_SOURCE=1` | delete this SF's source container **and** volume, then exit |
| `SRC_PORT` / `CLONE_PORT` | default `55610` / `55611` |
| `GFS_BIN` / `PSQL` / `GFS_IMAGE` | override paths |

### Managing persistent sources

```bash
docker ps -a --filter name=gfs-tpch-src      # which source DBs exist
docker volume ls | grep gfs-tpch             # their volumes (data)
DROP_SOURCE=1 SF=10 ./crates/extensions/gfs/tpch_validate.sh   # remove one
```

### Reading the output

Each section prints the route (`fetched` / `federated` / `local`) and a
`PASS clone == source` assertion; the final `Clone state` table shows per-table
`whole_cached` / `partial_rows` / `rows_fetched` / `federate_calls`. Verdict:
`N passed, M failed`.

---

## Notes / gotchas baked into the scripts

- **Don't calibrate at this scale.** `gfs.calibrate()` rewrites `net` to measured
  seconds/byte, which makes big tables *whole-ownable* and collapses the P2 partial
  niche. `tpch_validate.sh` pins fixed weights and does **not** calibrate.
- **Ceiling scales with the data.** The whole-own ceiling is set to `0.1 ×`
  lineitem's whole-own cost, so the big facts stay not-ownable (P2/P3) while a 5%
  slice still fits — a *fixed* ceiling doesn't generalize across scale factors.
- **Federation pushdown needs `use_remote_estimate`.** The foreign server is
  created with `use_remote_estimate 'true'` + `fetch_size '10000'` so joins/
  aggregates are pushed to the source; without it `postgres_fdw` fetches base rows
  over a cursor and joins locally (catastrophic at scale).

## Output files

- `benchmark.results.tsv` — append-only objective metrics per `LABEL` (router
  benchmark), used for the regression guard.

> The package `README.md` still describes the **old TAM** design; the current
> implementation is a `planner_hook` + real tables + cost-based router (see
> `src/lib.rs`). That README needs updating separately.
