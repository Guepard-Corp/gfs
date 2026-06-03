#!/usr/bin/env bash
# FAITHFUL copy-on-read probe: the SOURCE is a SEPARATE database reached via
# postgres_fdw (the clone's real architecture) — NOT a same-db table. The gfs
# TAM SPI-fetches the missing rows ACROSS the FDW and writes them through into
# the local heap. Decisive independence proof: after the first read, drop the
# foreign server entirely; the clone still returns every row, fully local.
# Same C code as probe_fed.sh — only the nature of gfs_remote.<t> changes
# (foreign table instead of plain table), which proves the stand-in was faithful.
# Ephemeral cluster on :55473. Assumes the extension is already built+installed.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
PGBIN="$(pg_config --bindir)"
DATA="$HERE/_pgdata_fdw"
PORT="${PORT:-55473}"
LOG="$HERE/_pg_fdw.log"

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

# qc = the CLONE database (postgres) ; qs = the SOURCE database (source_db)
qc() { "$PGBIN/psql" -p "$PORT" -U postgres -d postgres   -v ON_ERROR_STOP=1 -tAc "$1"; }
qs() { "$PGBIN/psql" -p "$PORT" -U postgres -d source_db  -v ON_ERROR_STOP=1 -tAc "$1"; }

step "SOURCE = a separate database 'source_db' with the full 10 rows"
qc "CREATE DATABASE source_db" >/dev/null
qs "CREATE TABLE fed (id bigint, name text)" >/dev/null
qs "INSERT INTO fed SELECT g,'n'||g FROM generate_series(1,10) g" >/dev/null

step "CLONE = db 'postgres': gfs table (3 local) + postgres_fdw to the remote"
qc "CREATE EXTENSION gfs" >/dev/null
qc "CREATE EXTENSION postgres_fdw" >/dev/null
qc "CREATE SERVER src FOREIGN DATA WRAPPER postgres_fdw OPTIONS (host 'localhost', port '$PORT', dbname 'source_db')" >/dev/null
qc "CREATE USER MAPPING FOR postgres SERVER src OPTIONS (user 'postgres')" >/dev/null
qc "CREATE SCHEMA gfs_remote" >/dev/null
qc "CREATE FOREIGN TABLE gfs_remote.fed (id bigint, name text) SERVER src OPTIONS (schema_name 'public', table_name 'fed')" >/dev/null
qc "CREATE TABLE fed (id bigint, name text) USING gfs" >/dev/null
qc "INSERT INTO fed VALUES (1,'n1'),(2,'n2'),(3,'n3')" >/dev/null
qc "SELECT gfs.register_clone('fed', 'gfs_remote.fed', 'id')" >/dev/null  # the API GFS calls

eq "the source is genuinely REMOTE (reachable only via the FDW): 10 rows" \
  "$(qc "SELECT count(*) FROM gfs_remote.fed")" "10"

step "First read of fed fetches the 7 missing rows ACROSS the FDW + serves all 10"
eq "first SELECT returns all 10 (3 local + 7 fetched via postgres_fdw)" \
  "$(qc "SELECT string_agg(name, ',' ORDER BY id) FROM fed")" "n1,n2,n3,n4,n5,n6,n7,n8,n9,n10"
eq "the 7 fetched rows were written through (now local)" \
  "$(qc "SELECT count(*) FROM fed WHERE id BETWEEN 4 AND 10")" "7"

step "INDEPENDENCE: drop the FDW server entirely, then re-read"
qc "DROP SERVER src CASCADE" >/dev/null   # also drops the foreign table gfs_remote.fed
eq "no foreign table / server remains" "$(qc "SELECT count(*) FROM information_schema.foreign_tables")" "0"
eq "remote source unreachable, fed STILL returns all 10 (clone is independent)" \
  "$(qc "SELECT count(*) FROM fed")" "10"
eq "data still complete after the remote is gone" \
  "$(qc "SELECT string_agg(name, ',' ORDER BY id) FROM fed")" "n1,n2,n3,n4,n5,n6,n7,n8,n9,n10"

echo
echo "================ RESULT: $PASS pass, $FAIL fail ================"
echo "Same TAM C code as probe_fed.sh; here gfs_remote.fed is a postgres_fdw"
echo "foreign table to a separate database — the clone's real architecture."
[ "$FAIL" -eq 0 ]
