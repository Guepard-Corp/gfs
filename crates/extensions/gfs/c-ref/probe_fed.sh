#!/usr/bin/env bash
# Step-2 probe: real COPY-ON-READ. A `gfs` table starts partially populated; the
# full data lives in a source table gfs_remote.<t>. On read, the missing rows are
# fetched (SPI), served to the triggering query, AND written through into the
# local heap — so afterwards the table is INDEPENDENT of the source (decisive
# proof: drop the source, the table still returns everything). Also checks the
# step-1 properties still hold (exact count, rescan-safe, no double write-through)
# and that a non-federated table stays pure heap.
# Ephemeral cluster on :55471 (distinct from the dev cluster :55470). Assumes the
# extension is already built+installed.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
PGBIN="$(pg_config --bindir)"
DATA="$HERE/_pgdata_fed"
PORT="${PORT:-55471}"
LOG="$HERE/_pg_fed.log"

PASS=0; FAIL=0
ok()  { echo "  PASS: $1"; PASS=$((PASS+1)); }
bad() { echo "  FAIL: $1"; FAIL=$((FAIL+1)); }
step(){ printf '\n== %s ==\n' "$1"; }
eq()  { [ "$2" = "$3" ] && ok "$1" || bad "$1 [want '$3' got '$2']"; }

cleanup() {
  step "Tear down"
  [ -f "$DATA/postmaster.pid" ] && "$PGBIN/pg_ctl" -D "$DATA" stop -m immediate >/dev/null 2>&1
  rm -rf "$DATA" "$LOG"
}
trap cleanup EXIT

step "Init throwaway cluster on :$PORT"
rm -rf "$DATA"
"$PGBIN/initdb" -D "$DATA" -U postgres >/dev/null 2>&1
"$PGBIN/pg_ctl" -D "$DATA" -o "-p $PORT -c logging_collector=off" -l "$LOG" start >/dev/null 2>&1
for _ in $(seq 1 30); do "$PGBIN/pg_isready" -p "$PORT" -U postgres >/dev/null 2>&1 && break; sleep 0.5; done

q()  { "$PGBIN/psql" -p "$PORT" -U postgres -d postgres -v ON_ERROR_STOP=1 -tAc "$1"; }
qo() { PGOPTIONS="$1" "$PGBIN/psql" -p "$PORT" -U postgres -d postgres -v ON_ERROR_STOP=1 -tAc "$2"; }

q "CREATE EXTENSION gfs" >/dev/null
q "CREATE SCHEMA gfs_remote" >/dev/null

step "Two gfs tables, each 3 local rows; the source has the full 10 rows"
for T in fed fed2; do
  q "CREATE TABLE $T (id bigint, name text) USING gfs" >/dev/null
  q "INSERT INTO $T VALUES (1,'n1'),(2,'n2'),(3,'n3')" >/dev/null            # 3 local
  q "CREATE TABLE gfs_remote.$T (id bigint, name text)" >/dev/null           # the source
  q "INSERT INTO gfs_remote.$T SELECT g,'n'||g FROM generate_series(1,10) g" >/dev/null
  q "SELECT gfs.register_clone('$T', 'gfs_remote.$T', 'id')" >/dev/null       # the API GFS calls
done

step "fed — first read fetches the 7 missing rows and serves them in THIS query"
WANT="n1,n2,n3,n4,n5,n6,n7,n8,n9,n10"
eq "first SELECT returns all 10 (3 local + 7 fetched), exact, in order" \
  "$(q "SELECT string_agg(name, ',' ORDER BY id) FROM fed")" "$WANT"

step "fed — the fetched rows were WRITTEN THROUGH (now physically local)"
eq "rows 4..10 are now local rows"            "$(q "SELECT count(*) FROM fed WHERE id BETWEEN 4 AND 10")" "7"
eq "count still exactly 10 (no double-count)" "$(q "SELECT count(*) FROM fed")" "10"

step "fed — INDEPENDENCE: drop the source, the table still has everything"
q "DROP TABLE gfs_remote.fed" >/dev/null
eq "source gone, count still 10 (clone is self-sufficient)" "$(q "SELECT count(*) FROM fed")" "10"
eq "source gone, data still complete" "$(q "SELECT string_agg(name, ',' ORDER BY id) FROM fed")" "$WANT"

step "fed2 — rescan re-emits fetched rows, WITHOUT re-fetching/double-writing"
PLAN="$(qo '-c enable_material=off -c enable_hashjoin=off -c enable_mergejoin=off' \
  "EXPLAIN (COSTS off) SELECT count(*) FROM (VALUES (1),(2),(3)) v(x), fed2" | tr '\n' '|')"
echo "    join plan: $PLAN"
eq "cross join 3 x fed2 = 30 (3 x (3 local + 7 fetched), rescan-safe)" \
  "$(qo '-c enable_material=off -c enable_hashjoin=off -c enable_mergejoin=off' \
       "SELECT count(*) FROM (VALUES (1),(2),(3)) v(x), fed2")" "30"
eq "fed2 now has exactly 10 local rows (no duplicate write-through)" "$(q "SELECT count(*) FROM fed2")" "10"
eq "id=7 persisted exactly once (not 3x)" "$(q "SELECT count(*) FROM fed2 WHERE id=7")" "1"

step "stats: the extension catalog records copy-on-read activity (gfs.clones)"
eq "fed: 7 rows fetched recorded"  "$(q "SELECT rows_fetched FROM gfs.clones WHERE clone='fed'")" "7"
eq "fed2: 7 rows fetched recorded" "$(q "SELECT rows_fetched FROM gfs.clones WHERE clone='fed2'")" "7"

step "Regression: a table with NO gfs_remote source stays pure heap"
q "CREATE TABLE t (id bigint PRIMARY KEY, name text NOT NULL) USING gfs" >/dev/null
q "INSERT INTO t SELECT g,'n'||g FROM generate_series(1,1000) g" >/dev/null
eq "t: count = 1000 (no federation)"        "$(q "SELECT count(*) FROM t")" "1000"
eq "t: PK index lookup id=42 -> n42"        "$(q "SELECT name FROM t WHERE id=42")" "n42"

echo
echo "================ RESULT: $PASS pass, $FAIL fail ================"
[ "$FAIL" -eq 0 ]
