#!/usr/bin/env bash
# PoC: can the planner ELIDE the foreign scan (zero remote contact) for a query
# whose key falls in a locally-cached range?
#
# Linchpin of the in-DB "stop re-reading the same rows from source" design.
#
# Mechanism under test: a CHECK constraint on the foreign table declaring the
# NON-cached ranges, plus constraint_exclusion=on, so the planner REFUTES a
# query qual that contradicts the CHECK and prunes the foreign scan entirely.
# Correctness for unconstrained scans (which the CHECK can't prune) is preserved
# by the overlay's NOT EXISTS anti-join.
#
# Proof of "no remote contact" = the remote's statement log (log_statement=all)
# shows NO query for a cached-range read, but DOES for a non-cached read.
#
# Requires Docker. Self-contained; cleans up on exit.
set -euo pipefail

NET=gfs-poc-elision-net
REMOTE=gfs-poc-elision-remote
LOCAL=gfs-poc-elision-local
IMG=postgres:16

cleanup() {
  docker rm -f "$REMOTE" "$LOCAL" >/dev/null 2>&1 || true
  docker network rm "$NET" >/dev/null 2>&1 || true
}
trap cleanup EXIT
cleanup

rpsql() { docker exec "$REMOTE" psql -U postgres -d postgres -v ON_ERROR_STOP=1 -tAc "$1"; }
lpsql() { docker exec "$LOCAL"  psql -U postgres -d postgres -v ON_ERROR_STOP=1 -tAc "$1"; }
lexplain() { docker exec "$LOCAL" psql -U postgres -d postgres -v ON_ERROR_STOP=1 -c "$1"; }

wait_ready() {
  local c=$1 deadline=$((SECONDS+60))
  until docker exec "$c" pg_isready -U postgres -d postgres >/dev/null 2>&1; do
    [ $SECONDS -lt $deadline ] || { echo "FAIL: $c never ready"; exit 1; }
    sleep 0.5
  done
}

echo "== Starting two postgres on a shared network =="
docker network create "$NET" >/dev/null
# Remote logs every statement so we can prove whether it was contacted.
docker run -d --name "$REMOTE" --network "$NET" \
  -e POSTGRES_PASSWORD=postgres "$IMG" \
  -c log_statement=all -c log_min_messages=warning >/dev/null
docker run -d --name "$LOCAL" --network "$NET" \
  -e POSTGRES_PASSWORD=postgres "$IMG" >/dev/null
wait_ready "$REMOTE"; wait_ready "$LOCAL"

echo "== Seed remote: orders(id bigint PK, name text), 10000 rows =="
rpsql "CREATE TABLE orders (id bigint PRIMARY KEY, name text NOT NULL);
       INSERT INTO orders SELECT g,'n'||g FROM generate_series(1,10000) g;" >/dev/null

echo "== Build overlay on local (fdw + foreign table + local store + view) =="
lpsql "ALTER DATABASE postgres SET constraint_exclusion = on;" >/dev/null
lpsql "CREATE EXTENSION IF NOT EXISTS postgres_fdw;
  CREATE SERVER remote_srv FOREIGN DATA WRAPPER postgres_fdw
    OPTIONS (host '$REMOTE', port '5432', dbname 'postgres');
  CREATE USER MAPPING FOR CURRENT_USER SERVER remote_srv
    OPTIONS (user 'postgres', password 'postgres');
  CREATE SCHEMA gfs_remote;
  IMPORT FOREIGN SCHEMA public LIMIT TO (orders) FROM SERVER remote_srv INTO gfs_remote;
  CREATE TABLE orders_local   (LIKE gfs_remote.orders);
  ALTER TABLE orders_local ADD PRIMARY KEY (id);
  CREATE TABLE orders_deleted (id bigint PRIMARY KEY);" >/dev/null

build_view() {
  lpsql "CREATE OR REPLACE VIEW orders AS
      SELECT * FROM orders_local
      UNION ALL
      SELECT r.* FROM gfs_remote.orders r
       WHERE NOT EXISTS (SELECT 1 FROM orders_local   l WHERE l.id = r.id)
         AND NOT EXISTS (SELECT 1 FROM orders_deleted d WHERE d.id = r.id);" >/dev/null
}
build_view

echo "== Warm range [1,1000] into the local store, then declare it on the FDW =="
lpsql "INSERT INTO orders_local SELECT * FROM gfs_remote.orders WHERE id BETWEEN 1 AND 1000
       ON CONFLICT DO NOTHING;" >/dev/null
# The CHECK declares 'this foreign table only holds rows OUTSIDE the cached range'.
# It is a deliberate lie wrt the data (the foreign table maps the whole remote),
# but constraint exclusion only uses it to PRUNE; the anti-join keeps unpruned
# scans correct.
lpsql "ALTER FOREIGN TABLE gfs_remote.orders
         ADD CONSTRAINT ck_not_cached CHECK (id < 1 OR id > 1000);" >/dev/null

# Reset remote log marker.
LOGMARK=$(docker logs "$REMOTE" 2>&1 | wc -l | tr -d ' ')
remote_saw() {  # arg: needle; echoes count of new remote log lines matching
  docker logs "$REMOTE" 2>&1 | tail -n +"$((LOGMARK+1))" | grep -c "$1" || true
}

echo
echo "================ TEST A: point read INSIDE cached range (id=42) ================"
echo "--- plan (expect the Foreign Scan pruned / never executed) ---"
PLAN_A=$(lexplain "EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF) SELECT * FROM orders WHERE id = 42;")
echo "$PLAN_A"
VAL_A=$(lpsql "SELECT name FROM orders WHERE id = 42;")
SAW_A=$(remote_saw "id = 42")

echo
echo "================ TEST B: point read OUTSIDE cached range (id=5000) =============="
echo "--- plan (expect the Foreign Scan executed, with Remote SQL) ---"
PLAN_B=$(lexplain "EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF) SELECT * FROM orders WHERE id = 5000;")
echo "$PLAN_B"
VAL_B=$(lpsql "SELECT name FROM orders WHERE id = 5000;")
SAW_B=$(remote_saw "id = 5000")

echo
echo "================ TEST C: correctness of an unconstrained scan ==================="
CNT=$(lpsql "SELECT count(*) FROM orders;")

echo
echo "================================ RESULTS ========================================"
fail=0
chk() { # desc, condition(0/1)
  if [ "$2" = "1" ]; then echo "  PASS  $1"; else echo "  FAIL  $1"; fail=1; fi
}

# A: value correct, served WITHOUT contacting the remote.
chk "A: id=42 returns correct value (n42)            [$VAL_A]" "$([ "$VAL_A" = "n42" ] && echo 1 || echo 0)"
# Strongest possible outcome: constraint exclusion removes the foreign branch
# from the plan entirely (no Foreign Scan / Remote SQL node at all).
chk "A: foreign scan pruned from plan for cached read (plan)" "$(echo "$PLAN_A" | grep -qiE 'Foreign Scan|Remote SQL' && echo 0 || echo 1)"
chk "A: remote was NOT contacted (0 log hits)         [hits=$SAW_A]" "$([ "$SAW_A" = "0" ] && echo 1 || echo 0)"

# B: value correct, served BY contacting the remote.
chk "B: id=5000 returns correct value (n5000)         [$VAL_B]" "$([ "$VAL_B" = "n5000" ] && echo 1 || echo 0)"
chk "B: foreign scan executed for non-cached read     (plan)" "$(echo "$PLAN_B" | grep -qi 'Remote SQL' && echo 1 || echo 0)"
chk "B: remote WAS contacted (>=1 log hit)            [hits=$SAW_B]" "$([ "$SAW_B" -ge 1 ] && echo 1 || echo 0)"

# C: unconstrained scan stays correct (no duplicates from the lying CHECK).
chk "C: count(*) = 10000 (anti-join dedups)           [$CNT]" "$([ "$CNT" = "10000" ] && echo 1 || echo 0)"

echo
if [ "$fail" = "0" ]; then
  echo "ALL PASS — cached-range reads are served locally with ZERO remote contact,"
  echo "non-cached reads still hit the remote, and scans remain correct."
else
  echo "SOME CHECKS FAILED — see above."
  exit 1
fi
