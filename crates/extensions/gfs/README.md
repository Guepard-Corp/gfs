# gfs — copy-on-read storage (Table Access Method), Rust/pgrx

**Why a TAM.** The overlay approach (views + FDW + `search_path`) cannot be fully
transparent to apps: `public.orders` resolves to a *view* (`relkind='v'`),
`SELECT … FOR UPDATE` is rejected, an app's own `SET search_path` breaks, and
ORMs/introspection see `gfs_remote_*` / `gfs_ovl__*`. A **Table Access Method**
makes the faithful table a *real* table (`relkind='r'`) whose missing rows are
fetched from the source on read and **written through** locally — the app cannot
tell the clone from an ordinary database ("a replica, but lazy / copy-on-read").

**Approach.** pgrx has no high-level TAM support, so this is built directly on
`pgrx::pg_sys` (raw FFI — the storage hot path is C-level either way). The handler
copies heap's routine (`GetHeapamTableAmRoutine`) and overrides only the scan/
index path; everything else stays heap's. Copy-on-read is driven by the
extension's catalog (`gfs.clone_source`), not hard-coded.

## Build / test

Built with **cargo-pgrx** (it is excluded from the parent Cargo workspace):

```bash
cd crates/extensions/gfs
cargo pgrx install --pg-config "$(which pg_config)"   # build + install into a local PG
docker build -t gfs-postgres:16 .                     # or package into an image (slow, multi-stage)
```

## What it does (validated end-to-end)

- `CREATE EXTENSION gfs` + `CREATE ACCESS METHOD gfs`; `CREATE TABLE … USING gfs`
  is a real `relkind='r'` table with full CRUD, PK/index, bitmap/seq scans.
- `SELECT gfs.register_clone('orders', 'gfs_remote.orders', 'id')` marks it a
  copy-on-read clone of a source relation (a postgres_fdw foreign table in a real
  clone). On read, missing rows are fetched (SPI) and `heap_insert`-written
  through (bypassing FK/RI, so a child read doesn't trip a not-yet-local parent),
  with index maintenance. After the first read the table is independent of the
  source. Activity lands in `gfs.clone_stats` (view: `gfs.clones`).
- `gfs.warm(rel)` force-materializes a clone in full (a seq scan).

## API (catalog + functions)

`gfs.clone_source` / `gfs.clone_stats` / `gfs.clones`, and
`gfs.register_clone(local, source_ref, key_col)` / `gfs.unregister_clone(local)`
/ `gfs.warm(local)`. GFS calls these from `clone_bootstrap.sql`.

## Files

- `src/lib.rs` — the extension (TAM via `pg_sys`, catalog/API via `extension_sql!`).
- `Cargo.toml`, `.cargo/config.toml`, `gfs.control` — pgrx crate config.
- `Dockerfile` — package into a `postgres:16` image with the extension.
- `c-ref/` — the original C/PGXS implementation (reference) + its probe scripts.

## Limits (PoC; hardening before prod)

The storage hot path is `unsafe` FFI (expected — no high-level TAM API). Single
key column (composite PK = TODO); `source_ref`/`key_col` are admin-set (quote/
validate for injection); the per-scan metadata SPI could be cached; write-through
needs a writable (non-standby) cluster; federation rides the seq-scan path only.
