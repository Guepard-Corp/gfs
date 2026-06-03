# PoC: transitive FK warming for fast JOINs (level 0, pure SQL)

**Question.** The overlay (`local UNION ALL foreign − anti-join`) kills
`postgres_fdw`'s join pushdown, so a cold JOIN federates two full scans. Can we
make joins local **without** a custom extension, by exploiting the PK/FK that
`pg_dump --schema-only` already replays onto the faithful store?

**Answer: yes, for OLTP / star-schema joins.** `run.sh` (two postgres
containers, reproducing the `clone_bootstrap.sql` overlay shape) — 6/6 PASS.

## Findings

1. **Transitive FK warming works, but topological order is mandatory.** The FK
   is real on the faithful store (a *partial* set of rows), so warming a CHILD
   before its PARENT raises `violates foreign key constraint`. Warm PARENTS
   first, under `session_replication_role = replica` (which `warm_range` already
   sets — it disables FK triggers during the bulk copy). Control E confirms a
   naive child-first warm is rejected.

2. **Join-side elision differs by role.** The existing key-range/membership
   CHECK elides the CHILD (the query has a direct qual `o.id BETWEEN …`), but
   **never the PARENT**: a JOIN gives the planner no direct qual on `c.id`
   (customers is filtered *by the join*), so a CHECK on `c.id` can't be refuted.
   The correct lever for a fully-cached dimension table is **whole_table
   promotion** — rewrite the overlay view to `SELECT * FROM public.<t>`, dropping
   the foreign branch. Result: 0 Foreign Scan on the join (experiment C).

3. This fits the star schema: dimension tables are small, so whole_table
   promotion is cheap. `maybe_promote_whole` / `fully_cached` already exist in
   the bootstrap — level 0 is about **triggering them via FKs** when a child is
   warmed.

## Implication for the design

- **Level 0 (pure SQL, no extension):** on warming table T, read T's outgoing
  FKs (`pg_constraint contype='f'`, already present), warm referenced parent
  rows in topological order (parents first, under `replica`), and promote small
  parents to whole_table so their Foreign Scan is elided in joins. Bounded
  recursion for FK cycles. Covers OLTP / star-schema joins.
- **Level 1 (FDW pushdown, extension):** still needed only for purely cold
  analytical joins over large tables (no warmed child to seed the transitive
  walk). PoC'd separately.

## Run

```bash
bash run.sh   # ~30-60s; needs Docker. Cleans up its own containers on exit.
```

⚠️ If the run is killed (e.g. by a timeout) before its `trap cleanup` fires, the
`poc-join-fk-*` containers stay up. Remove them:
`docker rm -f poc-join-fk-src poc-join-fk-clone; docker network rm poc-join-fk-net`.
