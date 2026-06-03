#!/usr/bin/env bash
# PoC: ROW-LEVEL read-through cache ("fetch each source row once, then read it
# locally forever"). The user's core requirement for the gfs_clone extension.
#
# THE question this must answer empirically:
#   After a row is cached locally, does a SECOND read of it avoid the source —
#   or does the overlay still probe the remote per-row (anti-join discards the
#   remote row only AFTER the round-trip)?
#
# If pure-SQL overlay still re-probes the remote for already-cached rows, then
# row-level "read locally, zero source contact" is IMPOSSIBLE without a custom
# scan (FDW that checks local first) — which is the real justification for the
# extension. We MEASURE this, we don't assume it.
#
# Single container: postgres_fdw loops back to the SAME db; schema `src` is "the
# source", `rmt` are foreign tables over it, `loc` is the local store, and a
# view per table is the overlay. Source contact is counted from the server log
# (statements naming src.<table>). One container -> no multi-container wedge.
set -uo pipefail

C=poc-rowcache
docker rm -f "$C" >/dev/null 2>&1
trap 'docker rm -f "$C" >/dev/null 2>&1' EXIT
docker run -d --name "$C" -e POSTGRES_PASSWORD=postgres -e POSTGRES_DB=gfs \
  postgres:16 -c log_statement=all -c log_min_messages=warning >/dev/null
for _ in $(seq 1 60); do docker exec "$C" pg_isready -U postgres >/dev/null 2>&1 && break; sleep 0.5; done

q() { docker exec -i "$C" psql -U postgres -d gfs -v ON_ERROR_STOP=1 -tAc "$1"; }
x() { docker exec -i "$C" psql -U postgres -d gfs -v ON_ERROR_STOP=1 -c "$1" >/dev/null; }

PASS=0; FAIL=0
ok()  { echo "  PASS: $1"; PASS=$((PASS+1)); }
bad() { echo "  FAIL: $1"; FAIL=$((FAIL+1)); }

# Count, in the server log, statements that hit the underlying source table.
# We tag each measurement window with a marker line in the log via a harmless
# query, then count src.customers reads after the marker.
src_reads_since() {  # $1 = marker
  docker logs "$C" 2>&1 | awk -v m="$1" '
    index($0,m){seen=1; next}
    seen && /statement:/ && /src\.customers/ {c++}
    END{print c+0}'
}
mark() { docker exec "$C" psql -U postgres -d gfs -tAc "SELECT 'MARK_$1'" >/dev/null 2>&1; }

echo "== set up source (src): customers=100000 (BIG), orders=small with FK =="
x "CREATE SCHEMA src;
   CREATE TABLE src.customers (id bigint PRIMARY KEY, name text NOT NULL, city text NOT NULL);
   CREATE TABLE src.orders (id bigint PRIMARY KEY, customer_id bigint NOT NULL REFERENCES src.customers(id), total numeric NOT NULL);
   INSERT INTO src.customers SELECT g,'cust'||g,'city'||(g%50) FROM generate_series(1,100000) g;
   INSERT INTO src.orders    SELECT g,((g-1)%100000)+1,(g*1.5) FROM generate_series(1,100000) g;"

echo "== overlay: rmt.* (foreign), loc.* (local store), view per table =="
x "CREATE EXTENSION postgres_fdw;
   CREATE SERVER s FOREIGN DATA WRAPPER postgres_fdw OPTIONS (host 'localhost', port '5432', dbname 'gfs');
   CREATE USER MAPPING FOR postgres SERVER s OPTIONS (user 'postgres', password 'postgres');
   CREATE SCHEMA rmt; IMPORT FOREIGN SCHEMA src LIMIT TO (customers, orders) FROM SERVER s INTO rmt;
   CREATE SCHEMA loc;
   CREATE TABLE loc.customers (LIKE src.customers INCLUDING ALL);
   CREATE TABLE loc.orders    (LIKE src.orders    INCLUDING DEFAULTS INCLUDING INDEXES);
   CREATE VIEW v_customers AS
     SELECT * FROM loc.customers
     UNION ALL SELECT r.* FROM rmt.customers r WHERE NOT EXISTS (SELECT 1 FROM loc.customers l WHERE l.id=r.id);
   CREATE VIEW v_orders AS
     SELECT * FROM loc.orders
     UNION ALL SELECT r.* FROM rmt.orders r WHERE NOT EXISTS (SELECT 1 FROM loc.orders l WHERE l.id=r.id);"

# The workload: a join over 200 specific orders -> touches 200 distinct customers
# out of 100000. We want ONLY those 200 customers to ever be fetched, once.
JOIN="SELECT o.id, c.name FROM v_orders o JOIN v_customers c ON c.id=o.customer_id WHERE o.id BETWEEN 1 AND 200"

echo
echo "############ ROUND 1: cold join, then ROW-LEVEL write-through ############"
mark R1
R1_PLAN=$(q "EXPLAIN (VERBOSE) $JOIN" 2>&1)
echo "$R1_PLAN" | grep -iE "Foreign Scan|Remote SQL|Nested Loop|Hash Join" | sed 's/^/    R1| /'

# Simulate the write-through a custom scan would do: cache exactly the rows the
# query reads. Child range first (orders 1..200), then the referenced parents by
# PK (the FK tells us which) -- batched, ON CONFLICT DO NOTHING (fetch once).
x "INSERT INTO loc.orders    SELECT * FROM rmt.orders    WHERE id BETWEEN 1 AND 200 ON CONFLICT DO NOTHING;
   INSERT INTO loc.customers SELECT * FROM rmt.customers
     WHERE id IN (SELECT customer_id FROM loc.orders WHERE id BETWEEN 1 AND 200) ON CONFLICT DO NOTHING;"
echo "    cached: loc.orders=$(q 'SELECT count(*) FROM loc.orders') loc.customers=$(q 'SELECT count(*) FROM loc.customers') (of 100000)"

echo
echo "############ ROUND 2: SAME join again -- does it still hit the source? #####"
mark R2
R2_PLAN=$(q "EXPLAIN (VERBOSE) $JOIN" 2>&1)
echo "$R2_PLAN" | grep -iE "Foreign Scan|Remote SQL|Index|Seq Scan|Nested Loop|Hash Join" | sed 's/^/    R2| /'
# Actually execute it (EXPLAIN alone may not contact remote the same way).
q "$JOIN" >/dev/null
R2_SRC=$(src_reads_since MARK_R2)
echo "    source (src.customers) reads during ROUND 2: $R2_SRC"

echo
echo "############ CORRECTNESS: overlay result == source ########################"
OVL=$(q "SELECT count(*), coalesce(sum(length(c.name)),0) FROM v_orders o JOIN v_customers c ON c.id=o.customer_id WHERE o.id BETWEEN 1 AND 200")
SRC=$(q "SELECT count(*), coalesce(sum(length(c.name)),0) FROM src.orders o JOIN src.customers c ON c.id=o.customer_id WHERE o.id BETWEEN 1 AND 200")
echo "    overlay=$OVL  source=$SRC"
[ "$OVL" = "$SRC" ] && ok "join result correct after row-level caching" || bad "result diverged"

echo
echo "############ THE VERDICT ###################################################"
if [ "$R2_SRC" -eq 0 ]; then
  ok "row-level cache alone gives ZERO source contact on re-read (pure SQL suffices!)"
else
  bad "re-read STILL hit the source $R2_SRC times despite rows being local"
  echo "    => pure-SQL overlay CANNOT do 'read once, then local' at row level:"
  echo "       the foreign branch is re-probed; the anti-join filters AFTER the"
  echo "       round-trip. A custom scan (FDW local-first) is REQUIRED -> extension."
fi

# For contrast: a BIG table whole-copy is what we're trying to avoid. Show the
# cache stayed tiny (200 of 100000), i.e. we did NOT bulk-copy.
CACHED=$(q "SELECT count(*) FROM loc.customers")
[ "$CACHED" -le 1000 ] && ok "cache stayed row-granular ($CACHED rows, not 100000 — no bulk copy)" \
                        || bad "cache ballooned ($CACHED) — that's bulk copy, not row-level"

echo
echo "================ RESULT: $PASS pass, $FAIL fail ================"
echo "(The VERDICT line above is the finding, not a pass/fail of the feature.)"
