#!/usr/bin/env bash
# FK probe: the real clone case. Faithful tables carry the source's FOREIGN KEYS.
# Copy-on-read must write-through a CHILD row even when its PARENT isn't local yet
# (the source already guaranteed integrity; the parent arrives on its own read).
# heap_insert bypasses RI/triggers, so this works; an INSERT…SELECT write-through
# would re-fire the FK and error. Then a join warms both sides and is correct.
# Ephemeral cluster on :55475. Assumes the extension is already built+installed.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
PGBIN="$(pg_config --bindir)"
DATA="$HERE/_pgdata_fk"
PORT="${PORT:-55475}"
LOG="$HERE/_pg_fk.log"

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

q() { "$PGBIN/psql" -p "$PORT" -U postgres -d postgres -v ON_ERROR_STOP=1 -tAc "$1"; }
# run a statement, capture stderr; used to assert "no error"
tryq() { "$PGBIN/psql" -p "$PORT" -U postgres -d postgres -v ON_ERROR_STOP=1 -tAc "$1" 2>&1; }

q "CREATE EXTENSION gfs" >/dev/null
q "CREATE SCHEMA gfs_remote" >/dev/null

step "SOURCE: customers (5) and orders (10) with orders.customer_id -> customers.id"
q "CREATE TABLE gfs_remote.customers (id bigint PRIMARY KEY, name text)" >/dev/null
q "INSERT INTO gfs_remote.customers SELECT g,'c'||g FROM generate_series(1,5) g" >/dev/null
q "CREATE TABLE gfs_remote.orders (id bigint PRIMARY KEY, customer_id bigint, amt int)" >/dev/null
q "INSERT INTO gfs_remote.orders SELECT g, 1+(g%5), g*10 FROM generate_series(1,10) g" >/dev/null

step "CLONE: faithful gfs tables WITH the FK, both empty, both registered"
q "CREATE TABLE customers (id bigint PRIMARY KEY, name text) USING gfs" >/dev/null
q "CREATE TABLE orders (id bigint PRIMARY KEY,
                        customer_id bigint REFERENCES customers(id),
                        amt int) USING gfs" >/dev/null
q "SELECT gfs.register_clone('customers', 'gfs_remote.customers', 'id')" >/dev/null
q "SELECT gfs.register_clone('orders',    'gfs_remote.orders',    'id')" >/dev/null

step "Read the CHILD first — write-through must NOT fire the FK (parent not local)"
OUT="$(tryq "SELECT gfs.warm('orders')")"
eq "warming child 'orders' returns 10 (no FK violation)" "$OUT" "10"
case "$OUT" in *violat*|*foreign\ key*|*ERROR*) bad "FK error leaked: $OUT";; esac

step "Lazy: the child read did NOT warm the parent"
eq "orders fetched 10"            "$(q "SELECT rows_fetched FROM gfs.clones WHERE clone='orders'")" "10"
eq "customers fetched 0 (lazy)"   "$(q "SELECT rows_fetched FROM gfs.clones WHERE clone='customers'")" "0"
eq "orders data correct (sum amt)" "$(q "SELECT sum(amt) FROM orders")" "550"

step "A join warms the parent and is correct"
eq "warm parent 'customers' -> 5" "$(q "SELECT gfs.warm('customers')")" "5"
eq "orders JOIN customers -> 10 rows, all matched" \
   "$(q "SELECT count(*) FROM orders o JOIN customers c ON c.id = o.customer_id")" "10"
eq "join picks up parent names" \
   "$(q "SELECT c.name FROM orders o JOIN customers c ON c.id=o.customer_id WHERE o.id=6")" "c2"

step "INDEPENDENCE: drop the source, both tables stand alone"
q "DROP SCHEMA gfs_remote CASCADE" >/dev/null
eq "orders still 10"    "$(q "SELECT count(*) FROM orders")" "10"
eq "customers still 5"  "$(q "SELECT count(*) FROM customers")" "5"
eq "join still correct" "$(q "SELECT count(*) FROM orders o JOIN customers c ON c.id=o.customer_id")" "10"

echo
echo "================ RESULT: $PASS pass, $FAIL fail ================"
[ "$FAIL" -eq 0 ]
