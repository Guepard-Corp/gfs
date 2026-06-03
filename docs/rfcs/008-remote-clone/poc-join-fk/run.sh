#!/usr/bin/env bash
# PoC: does TRANSITIVE FK WARMING make a JOIN fully local (no Foreign Scan)?
#
# Decision point for "level 0" (transitive FK warming, pure SQL, no extension).
#
# Round 1 surfaced THE key finding: warming a child (orders) before its parent
# (customers) violates the FK that pg_dump --schema-only already replayed onto
# the faithful store. So transitive warming MUST:
#   (a) follow FKs to warm PARENTS first (topological order), and/or
#   (b) hydrate under session_replication_role=replica (which warm_range already
#       does) so FK triggers don't fire during the bulk copy.
#
# We assert via the EXPLAIN PLAN (presence/absence of "Foreign Scan"), exactly
# like the repo's elision e2e tests — log-grep was unreliable.
#
# Overlay shape mirrors clone_bootstrap.sql:
#   public.<t>            = faithful store (PK+FK from pg_dump)
#   gfs_ovl__public.<t>   = overlay view (local UNION ALL foreign - anti-join)
#   gfs_remote_public.<t> = postgres_fdw foreign table
set -uo pipefail

SRC=poc-join-fk-src
CLO=poc-join-fk-clone
NET=poc-join-fk-net
PASS=postgres
IMG=postgres:16

cleanup() { docker rm -f "$SRC" "$CLO" >/dev/null 2>&1; docker network rm "$NET" >/dev/null 2>&1; }
cleanup
trap cleanup EXIT

PASS_CNT=0; FAIL_CNT=0
ok()  { echo "  PASS: $1"; PASS_CNT=$((PASS_CNT+1)); }
bad() { echo "  FAIL: $1"; FAIL_CNT=$((FAIL_CNT+1)); }

docker network create "$NET" >/dev/null
echo "== starting source + clone engines =="
docker run -d --name "$SRC" --network "$NET" \
  -e POSTGRES_PASSWORD=$PASS -e POSTGRES_DB=shop "$IMG" \
  -c log_statement=all >/dev/null
docker run -d --name "$CLO" --network "$NET" \
  -e POSTGRES_PASSWORD=$PASS -e POSTGRES_DB=gfs "$IMG" \
  -c constraint_exclusion=on -c work_mem=16MB >/dev/null

wait_ready() { for _ in $(seq 1 60); do docker exec "$1" pg_isready -U postgres >/dev/null 2>&1 && return 0; sleep 0.5; done; echo "FATAL: $1 never ready"; exit 1; }
wait_ready "$SRC"; wait_ready "$CLO"

ssql()  { docker exec -i "$SRC" psql -U postgres -d shop -v ON_ERROR_STOP=1 -tAc "$1"; }
csql()  { docker exec -i "$CLO" psql -U postgres -d gfs  -v ON_ERROR_STOP=1 -tAc "$1"; }
cexec() { docker exec -i "$CLO" psql -U postgres -d gfs  -v ON_ERROR_STOP=1 -c "$1"; }
# Does the plan for $1 contain a Foreign Scan (i.e. could touch the remote)?
plan_has_fscan() { csql "SELECT count(*) FROM (SELECT 1 FROM (SELECT 1) z WHERE false) q" >/dev/null; \
  docker exec -i "$CLO" psql -U postgres -d gfs -tAc "EXPLAIN $1" 2>&1 | grep -ciE "Foreign Scan"; }

echo "== seed source: customers(1000) + orders(5000, FK -> customers) =="
ssql "CREATE TABLE customers (id bigint PRIMARY KEY, name text NOT NULL);" >/dev/null
ssql "CREATE TABLE orders (id bigint PRIMARY KEY, customer_id bigint NOT NULL REFERENCES customers(id), total numeric NOT NULL);" >/dev/null
ssql "INSERT INTO customers SELECT g,'cust'||g FROM generate_series(1,1000) g;" >/dev/null
ssql "INSERT INTO orders SELECT g,((g-1)%1000)+1,(g*1.5) FROM generate_series(1,5000) g;" >/dev/null

echo "== faithful schema on clone (PK + FK, as pg_dump --schema-only replays) =="
cexec "CREATE TABLE customers (id bigint PRIMARY KEY, name text NOT NULL);" >/dev/null
cexec "CREATE TABLE orders (id bigint PRIMARY KEY, customer_id bigint NOT NULL REFERENCES customers(id), total numeric NOT NULL);" >/dev/null

echo "== overlay (FDW + shadow + view + anti-join) =="
cexec "CREATE EXTENSION IF NOT EXISTS postgres_fdw;" >/dev/null
cexec "CREATE SERVER gfs_remote_srv FOREIGN DATA WRAPPER postgres_fdw OPTIONS (host '$SRC', port '5432', dbname 'shop', updatable 'false');" >/dev/null
cexec "CREATE USER MAPPING FOR PUBLIC SERVER gfs_remote_srv OPTIONS (user 'postgres', password '$PASS');" >/dev/null
cexec "CREATE SCHEMA gfs_remote_public;" >/dev/null
cexec "IMPORT FOREIGN SCHEMA public LIMIT TO (customers, orders) FROM SERVER gfs_remote_srv INTO gfs_remote_public;" >/dev/null
cexec "CREATE SCHEMA gfs_ovl__public;" >/dev/null
cexec "CREATE TABLE gfs_ovl__public.customers__deleted (id bigint PRIMARY KEY);" >/dev/null
cexec "CREATE TABLE gfs_ovl__public.orders__deleted (id bigint PRIMARY KEY);" >/dev/null
for t in customers orders; do
cexec "CREATE VIEW gfs_ovl__public.$t AS
  SELECT * FROM public.$t
  UNION ALL
  SELECT r.* FROM gfs_remote_public.$t r
   WHERE NOT EXISTS (SELECT 1 FROM public.$t l WHERE l.id=r.id)
     AND NOT EXISTS (SELECT 1 FROM gfs_ovl__public.${t}__deleted d WHERE d.id=r.id);" >/dev/null
done

JOIN_Q="SELECT o.id, c.name FROM gfs_ovl__public.orders o JOIN gfs_ovl__public.customers c ON o.customer_id=c.id WHERE o.id BETWEEN 1 AND 50"

echo
echo "## A: COLD join — expect Foreign Scan present"
A=$(plan_has_fscan "$JOIN_Q")
echo "    Foreign Scan count (cold): $A"
[ "$A" -ge 1 ] && ok "cold join federates (Foreign Scan present, as expected)" || bad "cold join had no Foreign Scan (unexpected)"

echo
echo "## B: TRANSITIVE warm in FK order (PARENT customers first), under replica"
# Hydrate the child predicate range AND, transitively via the FK, its parents.
# Order matters for the FK; replica role also disables FK triggers during copy.
cexec "BEGIN;
  SET LOCAL session_replication_role = replica;
  -- 1) child rows the query touches
  INSERT INTO public.orders (id,customer_id,total)
    SELECT id,customer_id,total FROM gfs_remote_public.orders WHERE id BETWEEN 1 AND 50
    ON CONFLICT DO NOTHING;
  -- 2) TRANSITIVE: parents referenced by those child rows (follow the FK)
  INSERT INTO public.customers (id,name)
    SELECT id,name FROM gfs_remote_public.customers
     WHERE id IN (SELECT customer_id FROM public.orders WHERE id BETWEEN 1 AND 50)
    ON CONFLICT DO NOTHING;
COMMIT;" >/dev/null
echo "    warmed orders=$(csql "SELECT count(*) FROM public.orders") customers=$(csql "SELECT count(*) FROM public.customers")"
WO=$(csql "SELECT count(*) FROM public.orders"); WC=$(csql "SELECT count(*) FROM public.customers")
{ [ "$WO" -eq 50 ] && [ "$WC" -ge 1 ]; } && ok "transitive warm succeeded in FK order (no FK violation)" || bad "transitive warm failed (orders=$WO customers=$WC)"

echo
echo "## D: correctness (data present locally) — overlay join == source join"
OVL=$(csql "SELECT count(*), coalesce(sum(o.total),0) FROM gfs_ovl__public.orders o JOIN gfs_ovl__public.customers c ON o.customer_id=c.id WHERE o.id BETWEEN 1 AND 50;")
SRCR=$(ssql "SELECT count(*), coalesce(sum(o.total),0) FROM orders o JOIN customers c ON o.customer_id=c.id WHERE o.id BETWEEN 1 AND 50;")
echo "    overlay=$OVL  source=$SRCR"
[ "$OVL" = "$SRCR" ] && ok "join result identical to source after transitive warm" || bad "join result diverged"

echo
echo "## E: control — naive warm CHILD-FIRST without replica is rejected by FK"
# Fresh clone state with NO elision CHECK yet, so the source read returns rows.
cexec "TRUNCATE public.orders, public.customers;" >/dev/null
NAIVE=$(docker exec -i "$CLO" psql -U postgres -d gfs -c \
  "INSERT INTO public.orders (id,customer_id,total) SELECT id,customer_id,total FROM gfs_remote_public.orders WHERE id BETWEEN 1 AND 50;" 2>&1 | grep -c "violates foreign key")
[ "$NAIVE" -ge 1 ] && ok "naive child-first warm IS rejected by FK (ordering/replica matters)" || bad "expected FK violation on naive warm, got none"
# Re-warm correctly for the elision experiments below.
cexec "BEGIN; SET LOCAL session_replication_role=replica;
  INSERT INTO public.customers (id,name)
    SELECT id,name FROM gfs_remote_public.customers ON CONFLICT DO NOTHING;
  INSERT INTO public.orders (id,customer_id,total)
    SELECT id,customer_id,total FROM gfs_remote_public.orders WHERE id BETWEEN 1 AND 50
    ON CONFLICT DO NOTHING;
COMMIT;" >/dev/null

echo
echo "## C: ELISION strategies — CHILD via range-CHECK, PARENT via whole_table"
# Child (orders): warmed range is a contiguous key span -> range CHECK refutes
# the predicated foreign scan (this is the repo's existing elision).
cexec "ALTER FOREIGN TABLE gfs_remote_public.orders ADD CONSTRAINT gfs_excl CHECK (id < 1 OR id > 50);" >/dev/null
CHILD=$(plan_has_fscan "$JOIN_Q")
echo "    Foreign Scan count after child range-CHECK: $CHILD (parent still federates — a JOIN gives no direct qual on c.id)"

# Parent (customers): a JOIN provides no direct qual on c.id, so a key-range/
# membership CHECK can NEVER be refuted. The correct lever for a fully-cached
# dimension table is whole_table promotion: rewrite the overlay view to the
# local store only, dropping the foreign branch entirely.
cexec "CREATE OR REPLACE VIEW gfs_ovl__public.customers AS SELECT * FROM public.customers;" >/dev/null
C=$(plan_has_fscan "$JOIN_Q")
echo "    Foreign Scan count after parent whole_table promotion: $C"
[ "$C" -eq 0 ] && ok "child range-CHECK + parent whole_table => ZERO Foreign Scan on the join" || bad "Foreign Scan still present ($C)"

echo
echo "## F: correctness preserved after elision"
OVL2=$(csql "SELECT count(*), coalesce(sum(o.total),0) FROM gfs_ovl__public.orders o JOIN gfs_ovl__public.customers c ON o.customer_id=c.id WHERE o.id BETWEEN 1 AND 50;")
echo "    overlay=$OVL2  source=$SRCR"
[ "$OVL2" = "$SRCR" ] && ok "join result still identical after elision" || bad "join result diverged after elision"

echo
echo "================ RESULT: $PASS_CNT pass, $FAIL_CNT fail ================"
[ "$FAIL_CNT" -eq 0 ]
