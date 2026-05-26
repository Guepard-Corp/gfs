# GFS clone demo — Drizzle + PostgreSQL 14

A small [Drizzle](https://orm.drizzle.team/) app whose schema deliberately
exercises the extensions GFS must mirror, used to validate `gfs clone`
end-to-end:

> run the app on a **source** DB → `gfs clone` it → point the **same app** at the
> clone → it keeps working.

## The schema (why it is challenging)

A multi-tenant product catalog with fuzzy search:

| Table | Key | Exercises |
|-------|-----|-----------|
| `tenants` | `uuid` PK (`uuid_generate_v4()`) | **uuid-ossp** |
| `products` | `uuid` PK | **hstore** attributes + GIN index, **pg_trgm** GIN index on `name` |
| `product_tags` | **composite** PK `(product_id, tag)` | composite-key overlay |
| `orders` | `uuid` PK | |
| `order_items` | **composite** PK `(order_id, product_id)` | composite-key overlay |
| `audit_log` | `IDENTITY` PK | **plpgsql** trigger writing **hstore** diffs |

Extensions present on the source (matching the target set):
`plpgsql`, `hstore`, `pg_stat_statements`, `uuid-ossp`, `pg_trgm`, `dblink`.

Every table has a primary key — the overlay clone needs one to build its
updatable view, so a key-less table would be silently skipped.

## Prerequisites

- Docker Desktop (the clone reaches the source via `host.docker.internal`)
- Node 20+
- A built `gfs` binary: `cargo build -p gfs-cli` from the repo root
  (the script defaults to `target/debug/gfs`; override with `GFS_BIN`).

## Run it

```bash
cd examples/clone-drizzle-demo
npm run demo          # = bash scripts/run-demo.sh
```

That script:

1. starts `postgres:14` (with `shared_preload_libraries=pg_stat_statements`),
2. applies `sql/*.sql` (extensions → schema → trigger) and seeds the catalog,
3. runs the app against the **source** (baseline),
4. `gfs clone --from postgres://app:app@host.docker.internal:55432/appdb --database-version 14 --port 55433`,
5. runs the **same app** against the clone at
   `postgres://postgres:postgres@localhost:55433/postgres`.

### Manual run

```bash
docker compose up -d
export DATABASE_URL=postgres://app:app@localhost:55432/appdb
npm install
npm run db:setup && npm run db:seed
npm run app                                   # baseline against source

../../target/debug/gfs clone \
  --from postgres://app:app@host.docker.internal:55432/appdb \
  --database-version 14 --port 55433 ./clone-repo

DATABASE_URL=postgres://postgres:postgres@localhost:55433/postgres npm run app
```

> The clone's database is GFS's local `postgres` DB (user/db `postgres`), **not**
> `appdb` — the overlay views live there in the `public` schema, so the app's
> queries are identical.

## What "works" means on the clone

The app prints **core checks** and **diagnostics**.

**Core checks** (must pass on both source and clone): reads across all tables,
pg_trgm fuzzy search, hstore filtering, composite-key joins, and writes
(insert + composite-key tag + update + read-back).

**Diagnostics** are expected to *differ* on an overlay clone, by design:

- **plpgsql audit trigger** — fires on the source's base table; the clone's
  overlay is a view with copy-on-write `INSTEAD OF` triggers, and source
  triggers are **not** mirrored, so `audit_log` does not grow for clone-side
  writes.
- **pg_stat_statements** — needs `shared_preload_libraries`, which the source
  sets but the GFS-provisioned clone does not; the extension exists but its view
  is empty/unavailable there.
- **dblink** — round-trips through the extension to prove it is installed.

Keep the source container running while using the clone: the clone is
**copy-on-read**, fetching rows from the source on first access.

## Findings (what this demo surfaced about the overlay clone)

Running the workload against the clone validated reads, fuzzy search, hstore
filtering, composite-key joins, and writes. It also surfaced behaviours worth
knowing when building apps on a clone:

1. **Server-side column `DEFAULT`s — fixed.** `IMPORT FOREIGN SCHEMA` imports
   `NOT NULL` **without** the `DEFAULT` expression, so a `NOT NULL DEFAULT now()`
   column (e.g. `products.created_at`) initially had no default on the clone and
   an `INSERT` that omitted it failed with a not-null violation. The clone
   bootstrap now mirrors the remote's column defaults onto the overlay **view**
   (so `NEW` is populated before the `INSTEAD OF` trigger fires) and the local
   store. This app relies on `defaultNow()` and works **unchanged** on the clone.
2. **Triggers/functions are not mirrored.** `audit_log` grows on the source
   (the plpgsql trigger fires on the base table) but not on the clone, whose
   overlay views only carry the copy-on-write `INSTEAD OF` triggers.
3. **`pg_stat_statements` needs `shared_preload_libraries`.** The extension is
   created on the clone but its view is unavailable, because the
   GFS-provisioned engine does not preload the library (the source does, via its
   `command:`).

`uuid-ossp`, `hstore`, `pg_trgm`, `dblink` and `plpgsql` are all mirrored and
functional on the clone.

## Cleanup

```bash
docker compose down -v
(cd clone-repo && ../../../target/debug/gfs compute stop); rm -rf clone-repo
```
