#!/usr/bin/env bash
# PoC: does postgres_fdw ALREADY push a JOIN down to the remote when both sides
# are foreign tables of the same server? If yes, "level 1" (cold analytical join
# pushdown) needs NO custom pgrx FDW — only a way to route a cold join to the
# gfs_remote_* foreign tables (the overlay view is what defeats pushdown today).
#
# Single container: postgres_fdw loops back to the SAME database (a separate
# schema acts as "the remote"). Avoids the multi-container wedge. One container,
# background-friendly, robust cleanup.
set -uo pipefail
C=poc-fdwjoin
docker rm -f "$C" >/dev/null 2>&1
trap 'docker rm -f "$C" >/dev/null 2>&1' EXIT
docker run -d --name "$C" -e POSTGRES_PASSWORD=postgres -e POSTGRES_DB=gfs postgres:16 >/dev/null
for _ in $(seq 1 60); do docker exec "$C" pg_isready -U postgres >/dev/null 2>&1 && break; sleep 0.5; done

q() { docker exec -i "$C" psql -U postgres -d gfs -v ON_ERROR_STOP=1 -tAc "$1"; }
x() { docker exec -i "$C" psql -U postgres -d gfs -v ON_ERROR_STOP=1 -c "$1"; }

PASS=0; FAIL=0
ok()  { echo "  PASS: $1"; PASS=$((PASS+1)); }
bad() { echo "  FAIL: $1"; FAIL=$((FAIL+1)); }

echo "== set up 'remote' tables in schema src =="
x "CREATE SCHEMA src;
   CREATE TABLE src.customers (id bigint PRIMARY KEY, name text NOT NULL);
   CREATE TABLE src.orders (id bigint PRIMARY KEY, customer_id bigint NOT NULL, total numeric NOT NULL);
   INSERT INTO src.customers SELECT g,'c'||g FROM generate_series(1,500) g;
   INSERT INTO src.orders SELECT g,((g-1)%500)+1,g*1.5 FROM generate_series(1,5000) g;" >/dev/null

echo "== foreign server looping back to the same db, import src as gfs_remote =="
x "CREATE EXTENSION postgres_fdw;
   CREATE SERVER loop FOREIGN DATA WRAPPER postgres_fdw
     OPTIONS (host 'localhost', port '5432', dbname 'gfs');
   CREATE USER MAPPING FOR postgres SERVER loop OPTIONS (user 'postgres', password 'postgres');
   CREATE SCHEMA gfs_remote;
   IMPORT FOREIGN SCHEMA src LIMIT TO (customers, orders) FROM SERVER loop INTO gfs_remote;" >/dev/null

echo
echo "## A: JOIN between two FOREIGN tables (same server) — is it pushed down?"
PLAN_A=$(q "EXPLAIN (VERBOSE) SELECT o.id, c.name FROM gfs_remote.orders o JOIN gfs_remote.customers c ON o.customer_id=c.id WHERE o.id BETWEEN 1 AND 50")
echo "$PLAN_A" | sed 's/^/    A| /'
# Pushdown signature: a single Foreign Scan whose Remote SQL contains a JOIN.
if echo "$PLAN_A" | grep -qiE "Relations:.*JOIN|Remote SQL.* JOIN "; then
  ok "postgres_fdw pushes the JOIN to the remote (Remote SQL contains JOIN)"
else
  bad "no join pushdown detected between foreign tables"
fi
# And there should be exactly one Foreign Scan node (the joined relation), not two.
NF=$(echo "$PLAN_A" | grep -ciE "Foreign Scan")
echo "    Foreign Scan nodes: $NF (1 = joined-and-pushed; 2 = joined locally)"
[ "$NF" -eq 1 ] && ok "single Foreign Scan node (join executed remotely)" || bad "expected 1 Foreign Scan node, got $NF"

echo
echo "## B: correctness — pushed join result == local computation on src"
R=$(q "SELECT count(*), coalesce(sum(o.total),0) FROM gfs_remote.orders o JOIN gfs_remote.customers c ON o.customer_id=c.id WHERE o.id BETWEEN 1 AND 50")
S=$(q "SELECT count(*), coalesce(sum(o.total),0) FROM src.orders o JOIN src.customers c ON o.customer_id=c.id WHERE o.id BETWEEN 1 AND 50")
echo "    foreign=$R  src=$S"
[ "$R" = "$S" ] && ok "join result correct" || bad "join result diverged"

echo
echo "## C: control — does an OVERLAY VIEW over the foreign table defeat pushdown?"
x "CREATE TABLE src_local_orders (LIKE src.orders INCLUDING ALL);
   CREATE TABLE src_local_customers (LIKE src.customers INCLUDING ALL);
   CREATE VIEW ovl_orders AS
     SELECT * FROM src_local_orders
     UNION ALL SELECT r.* FROM gfs_remote.orders r
       WHERE NOT EXISTS (SELECT 1 FROM src_local_orders l WHERE l.id=r.id);
   CREATE VIEW ovl_customers AS
     SELECT * FROM src_local_customers
     UNION ALL SELECT r.* FROM gfs_remote.customers r
       WHERE NOT EXISTS (SELECT 1 FROM src_local_customers l WHERE l.id=r.id);" >/dev/null
PLAN_C=$(q "EXPLAIN (VERBOSE) SELECT o.id, c.name FROM ovl_orders o JOIN ovl_customers c ON o.customer_id=c.id WHERE o.id BETWEEN 1 AND 50")
NFC=$(echo "$PLAN_C" | grep -ciE "Foreign Scan")
echo "    Foreign Scan nodes through the overlay views: $NFC"
[ "$NFC" -ge 2 ] && ok "overlay view DOES defeat pushdown (2+ Foreign Scans) — confirms the diagnosis" || bad "expected overlay to break pushdown"

echo
echo "================ RESULT: $PASS pass, $FAIL fail ================"
[ "$FAIL" -eq 0 ]
