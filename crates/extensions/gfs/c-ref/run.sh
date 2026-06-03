#!/usr/bin/env bash
# Build the gfs PoC against local postgresql@16, then prove milestone 1:
# a table USING gfs is a REAL relkind='r' table that behaves like heap.
# No Docker: uses a throwaway data dir + a temp postgres on a high port.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
PGBIN="$(pg_config --bindir)"
PGLIB="$(pg_config --pkglibdir)"
PGSHARE="$(pg_config --sharedir)"
DATA="$HERE/_pgdata"
PORT="${PORT:-55470}"
LOG="$HERE/_pg.log"

PASS=0; FAIL=0
ok()  { echo "  PASS: $1"; PASS=$((PASS+1)); }
bad() { echo "  FAIL: $1"; FAIL=$((FAIL+1)); }
step(){ printf '\n== %s ==\n' "$1"; }

cleanup() {
  step "Tear down"
  [ -f "$DATA/postmaster.pid" ] && "$PGBIN/pg_ctl" -D "$DATA" stop -m immediate >/dev/null 2>&1
  rm -rf "$DATA" "$LOG"
}
trap cleanup EXIT

step "Build + install the extension (PGXS)"
make -C "$HERE" clean >/dev/null 2>&1
if ! make -C "$HERE" 2>&1; then echo "BUILD FAILED"; exit 1; fi
# Install needs write access to the pg lib/share dirs (homebrew: user-writable).
if ! make -C "$HERE" install 2>&1; then echo "INSTALL FAILED"; exit 1; fi
ok "extension built + installed (.so in $PGLIB, sql in $PGSHARE)"

step "Init a throwaway cluster on :$PORT"
rm -rf "$DATA"
"$PGBIN/initdb" -D "$DATA" -U postgres >/dev/null 2>&1
"$PGBIN/pg_ctl" -D "$DATA" -o "-p $PORT -c logging_collector=off" -l "$LOG" start >/dev/null 2>&1
for _ in $(seq 1 30); do "$PGBIN/pg_isready" -p "$PORT" -U postgres >/dev/null 2>&1 && break; sleep 0.5; done

psql() { "$PGBIN/psql" -p "$PORT" -U postgres -d postgres -v ON_ERROR_STOP=1 -tAc "$1"; }

step "CREATE EXTENSION + CREATE ACCESS METHOD"
if psql "CREATE EXTENSION gfs" >/dev/null 2>&1; then
  ok "extension loads"
else
  bad "CREATE EXTENSION failed"; "$PGBIN/psql" -p "$PORT" -U postgres -d postgres -c "CREATE EXTENSION gfs"; exit 1
fi
AM=$(psql "SELECT amname FROM pg_am WHERE amname='gfs'")
[ "$AM" = "gfs" ] && ok "access method registered in pg_am" || bad "access method missing"

step "CREATE TABLE ... USING gfs — must be a REAL table (relkind='r')"
psql "CREATE TABLE t (id bigint PRIMARY KEY, name text NOT NULL) USING gfs" >/dev/null
RELKIND=$(psql "SELECT relkind FROM pg_class WHERE relname='t'")
AMOF=$(psql "SELECT a.amname FROM pg_class c JOIN pg_am a ON a.oid=c.relam WHERE c.relname='t'")
echo "    relkind=$RELKIND  am=$AMOF"
[ "$RELKIND" = "r" ] && ok "table is relkind='r' (a real table, not a view/foreign)" || bad "relkind is '$RELKIND', not 'r'"
[ "$AMOF" = "gfs" ] && ok "table uses the gfs access method" || bad "table not on gfs"

step "It behaves like heap: INSERT / SELECT / UPDATE / DELETE / index"
psql "INSERT INTO t SELECT g,'n'||g FROM generate_series(1,1000) g" >/dev/null
C=$(psql "SELECT count(*) FROM t")
[ "$C" = "1000" ] && ok "insert+count works ($C)" || bad "count=$C"
N=$(psql "SELECT name FROM t WHERE id=42")          # exercises the PK index
[ "$N" = "n42" ] && ok "index lookup works ($N)" || bad "index lookup got '$N'"
psql "UPDATE t SET name='X' WHERE id=42" >/dev/null
[ "$(psql "SELECT name FROM t WHERE id=42")" = "X" ] && ok "update works" || bad "update failed"
psql "DELETE FROM t WHERE id=42" >/dev/null
[ "$(psql "SELECT count(*) FROM t WHERE id=42")" = "0" ] && ok "delete works" || bad "delete failed"
[ "$(psql "SELECT count(*) FROM t")" = "999" ] && ok "final count correct (999)" || bad "final count wrong"

echo
echo "================ RESULT: $PASS pass, $FAIL fail ================"
echo "Milestone 1 = a custom TAM extension yields a real, fully-functional table."
echo "Next milestone: override scan_getnextslot for copy-on-read from the source."
[ "$FAIL" -eq 0 ]
