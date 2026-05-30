# PoC — faithful schema + side-schema overlay, proxy-routed

Validates an architecture for the remote clone that **keeps the in-DB copy-on-read
overlay** but stops letting the overlay VIEW occupy the table's real name — so that
the source's triggers, functions, indexes and constraints keep working.

Run: `./run.sh` (needs Docker; self-contained, cleans up on exit).

## Layout under test

| Object | Role | Owner |
|---|---|---|
| `<schema>.<table>` | **100% faithful real table** (= local store) with the source's triggers/indexes/constraints | in-DB |
| `gfs_ovl__<schema>.<table>` | federation **view** (`local UNION ALL foreign − local − deleted`) + `INSTEAD OF` copy-on-write | in-DB |
| `gfs_remote_<schema>` | foreign tables (postgres_fdw) | in-DB |
| `gfs_sync` | tombstones / metadata | in-DB |
| session `search_path` | **interleaved**: `gfs_ovl__shop, shop, gfs_ovl__audit, audit` | proxy (simulated here) |

The "proxy" is reduced to one job: interleave each overlay schema just before its
source schema, so an **unqualified** read resolves to the overlay (federated) while
writes / triggers / functions operate on the real faithful tables.

## What the run proves (all PASS)

- **T1 — reads federate.** With the interleaved `search_path`, an unqualified
  `SELECT count(*) FROM products` returns all 5 source rows while the faithful
  `shop.products` is still empty. Routing works.
- **T2 — copy-on-write fires the faithful triggers (the key win).**
  - `INSERT … VALUES (100,'new-widget',…)` through the overlay lands in
    `shop.products`; the source `BEFORE` trigger upper-cases it → `NEW-WIDGET`.
  - `UPDATE … WHERE id=2` of a *not-yet-local* (federated) row hydrates it then
    updates it → `GIZMO-2 / 999`.
  - The `AFTER` trigger fired for both, writing `audit.product_log` (cross-schema).
  - This is exactly what a view-in-the-table's-name could **not** do.
- **T3 / T4 — the residual is real and measurable.** A source function with its
  own `SET search_path = shop`, and a **schema-qualified** read, both bypass the
  overlay and see only local rows: `count = 2` vs overlay `= 6`. Silent undercount.
- **T5 — warming closes the residual.** Hydrating the table whole into the faithful
  table makes function, qualified read and overlay all agree (`6 / 6 / 6`).

## Findings for the RFC

1. **The shape is forced.** A plain faithful table can't read-through (no SELECT
   trigger); inheritance/partition over-returns (foreign `CHECK` isn't enforced at
   runtime → duplicates). Only a `UNION ALL … NOT EXISTS` **view** dedups correctly,
   and it can't share the faithful table's name → faithful schema + side overlay +
   search-path routing is the only runtime-correct in-DB combination.
2. **Writes are solved cleanly.** `INSTEAD OF` on the overlay performing the write
   into the real table gives copy-on-write **and** fires the source triggers, with
   real constraints enforced — in one mechanism.
3. **The residual is `search_path`-shaped, not data-shaped.** Anything that resolves
   table names against a fixed/qualified path (functions with `SET search_path`,
   schema-qualified SQL) reads the partial faithful table. It fails **silently**
   (wrong count, no error). Mitigation = warm those tables whole; completeness is the
   only real fix, regardless of architecture.
4. **New constraint surfaced: hydration must suppress triggers.** Bulk warming
   `INSERT … SELECT FROM foreign` into a faithful triggered table would fire the
   business triggers for every hydrated row. The PoC warms under
   `session_replication_role = replica` to suppress them — the real implementation
   must do the same (warming is not an application write).

## Open arbitrages (unchanged by the PoC)

- **Read routing**: `search_path` interleaving (simple, used here; silent bypass on
  qualified/function-internal refs) vs SQL rewriting in the proxy (robust, fragile).
- **Completeness policy**: which tables get eagerly warmed whole (those with FKs,
  UNIQUE, or read by triggers/functions) so their objects are correct sooner.
