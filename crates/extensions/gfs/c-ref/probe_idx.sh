#!/usr/bin/env bash
# Step-3 probe: the INDEX/PK path.
#  (3a) Index maintenance on write-through: an indexed federated table stays
#       consistent. After a seq read warms it, a FORCED index lookup finds the
#       written-through rows, and repeated reads never re-fetch/duplicate (the PK
#       would reject a duplicate insert, so no error == no re-fetch).
#  (#2) Documented limitation: a point lookup via the index path of a key that
#       was NEVER fetched returns nothing — a missing key has no local index
#       entry, so index_fetch_tuple is never called and the TAM can't self-drive
#       per-key copy-on-read. A seq scan warms it; afterwards the index works.
#       => federation rides the SEQ-SCAN path; the warming/planner layer decides
#          what to materialize; the index then follows via write-through (3a).
# Ephemeral cluster on :55474. Assumes the extension is already built+installed.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
PGBIN="$(pg_config --bindir)"
DATA="$HERE/_pgdata_idx"
PORT="${PORT:-55474}"
LOG="$HERE/_pg_idx.log"

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

q()    { "$PGBIN/psql" -p "$PORT" -U postgres -d postgres -v ON_ERROR_STOP=1 -tAc "$1"; }
# warm: a SEQ read (index plans off) so federation triggers on scan_begin.
warm() { PGOPTIONS='-c enable_indexscan=off -c enable_indexonlyscan=off -c enable_bitmapscan=off' \
         "$PGBIN/psql" -p "$PORT" -U postgres -d postgres -v ON_ERROR_STOP=1 -tAc "SELECT count(*) FROM $1"; }
# idx: force an index plan.
idx()  { PGOPTIONS='-c enable_seqscan=off -c enable_bitmapscan=off' \
         "$PGBIN/psql" -p "$PORT" -U postgres -d postgres -v ON_ERROR_STOP=1 -tAc "$1"; }

q "CREATE EXTENSION gfs" >/dev/null
q "CREATE SCHEMA gfs_remote" >/dev/null

mk() { # $1 = table name : indexed gfs table (3 local) + source (10 rows)
  q "CREATE TABLE $1 (id bigint PRIMARY KEY, name text) USING gfs" >/dev/null
  q "INSERT INTO $1 VALUES (1,'n1'),(2,'n2'),(3,'n3')" >/dev/null
  q "CREATE TABLE gfs_remote.$1 (id bigint PRIMARY KEY, name text)" >/dev/null
  q "INSERT INTO gfs_remote.$1 SELECT g,'n'||g FROM generate_series(1,10) g" >/dev/null
  q "SELECT gfs.register_clone('$1', 'gfs_remote.$1', 'id')" >/dev/null
}

step "(3a) seq read warms an INDEXED federated table; index stays consistent"
mk fedi
eq "warm (seq) fetches the 7 missing -> 10"           "$(warm fedi)" "10"
eq "forced INDEX lookup finds a written-through row"  "$(idx "SELECT name FROM fedi WHERE id=7")" "n7"
eq "plan really is an Index Scan"                      "$(idx "EXPLAIN (COSTS off) SELECT name FROM fedi WHERE id=7" | grep -c 'Index Scan')" "1"

step "(3a) repeated reads do not re-fetch / duplicate (PK would reject a dup)"
warm fedi >/dev/null; warm fedi >/dev/null
eq "still exactly 10 (no duplicate write-through)" "$(q "SELECT count(*) FROM fedi")" "10"
eq "id=7 present exactly once"                     "$(q "SELECT count(*) FROM fedi WHERE id=7")" "1"
eq "fetched range 4..10 each present once"         "$(q "SELECT count(*) FROM fedi WHERE id BETWEEN 4 AND 10")" "7"

step "(#2) the index path cannot self-drive copy-on-read of a never-fetched key"
mk fedj
eq "index-only first touch of a missing key -> <none> (documented limit)" \
   "$(idx "SELECT coalesce((SELECT name FROM fedj WHERE id=7),'<none>')")" "<none>"
eq "after a SEQ scan warms it, the same index lookup works" \
   "$(warm fedj >/dev/null; idx "SELECT name FROM fedj WHERE id=7")" "n7"

step "Regression: a table with NO gfs_remote source stays pure heap"
q "CREATE TABLE t (id bigint PRIMARY KEY, name text NOT NULL) USING gfs" >/dev/null
q "INSERT INTO t SELECT g,'n'||g FROM generate_series(1,1000) g" >/dev/null
eq "t: count = 1000"                 "$(q "SELECT count(*) FROM t")" "1000"
eq "t: PK index lookup id=42 -> n42" "$(idx "SELECT name FROM t WHERE id=42")" "n42"

echo
echo "================ RESULT: $PASS pass, $FAIL fail ================"
[ "$FAIL" -eq 0 ]
