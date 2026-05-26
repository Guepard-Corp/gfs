#!/usr/bin/env bash
# Validates the OVERLAY approach (RFC 008) for correctness on the cases that
# broke the partition approach: global count, non-key predicate, update
# copy-on-write, delete tombstone, insert, and divergence vs the remote.
set -euo pipefail
cd "$(dirname "$0")"

cleanup() { docker compose down -v >/dev/null 2>&1 || true; }
trap cleanup EXIT

echo "==> up remote + gfs"
docker compose up -d --wait >/dev/null 2>&1
echo "==> seed remote (01-remote-seed.sql, 30000 rows)"
docker compose exec -T remote psql -U postgres -d shop -v ON_ERROR_STOP=1 < 01-remote-seed.sql >/dev/null
echo "==> overlay setup on gfs (FDW + local store + tombstones + view + triggers)"
docker compose exec -T gfs psql -U postgres -d gfs -v ON_ERROR_STOP=1 < 02-overlay-setup.sql >/dev/null

g() { docker compose exec -T gfs psql -U postgres -d gfs -tAc "$1"; }
r() { docker compose exec -T remote psql -U postgres -d shop -tAc "$1"; }
ok=1
chk() { # chk "label" actual expected
  if [ "$2" = "$3" ]; then echo "PASS: $1 ($2)"; else echo "FAIL: $1: got $2, want $3"; ok=0; fi
}

echo; echo "===== correctness BEFORE any hydration ====="
chk "global count = remote (no double count)" "$(g 'SELECT count(*) FROM orders')" "30000"
chk "non-key predicate count (customer=cust_500)" "$(g "SELECT count(*) FROM orders WHERE customer='cust_500'")" "30"
chk "selective key read served correctly" "$(g 'SELECT customer FROM orders WHERE id=12345')" "$(r 'SELECT customer FROM orders WHERE id=12345')"

echo; echo "===== hydrate range [10001,20001) (optimisation, not required for correctness) ====="
g "INSERT INTO orders_local (id,customer,amount,created_at)
   SELECT id,customer,amount,created_at FROM gfs_remote.orders WHERE id >= 10001 AND id < 20001
   ON CONFLICT (id) DO NOTHING" >/dev/null
chk "global count unchanged after hydration" "$(g 'SELECT count(*) FROM orders')" "30000"
chk "10000 rows now stored locally" "$(g 'SELECT count(*) FROM orders_local')" "10000"
chk "non-key count still correct after hydration" "$(g "SELECT count(*) FROM orders WHERE customer='cust_500'")" "30"

echo; echo "===== WRITE: UPDATE copy-on-write on remote-only range [25001,25050] ====="
g "UPDATE orders SET customer='X' WHERE id BETWEEN 25001 AND 25050" >/dev/null
chk "view shows 50 updated rows" "$(g "SELECT count(*) FROM orders WHERE customer='X'")" "50"
chk "remote untouched (0 'X')" "$(r "SELECT count(*) FROM orders WHERE customer='X'")" "0"
chk "global count unchanged by update" "$(g 'SELECT count(*) FROM orders')" "30000"

echo; echo "===== WRITE: DELETE (tombstone) a remote-only row id=29000 ====="
g "DELETE FROM orders WHERE id=29000" >/dev/null
chk "row hidden in view" "$(g 'SELECT count(*) FROM orders WHERE id=29000')" "0"
chk "global count decremented" "$(g 'SELECT count(*) FROM orders')" "29999"
chk "remote still has the row" "$(r 'SELECT count(*) FROM orders WHERE id=29000')" "1"

echo; echo "===== WRITE: INSERT a brand-new key id=40000 ====="
g "INSERT INTO orders (id,customer,amount,created_at) VALUES (40000,'brand_new',9.99,now())" >/dev/null
chk "new row visible in view" "$(g "SELECT customer FROM orders WHERE id=40000")" "brand_new"
chk "global count back to 30000" "$(g 'SELECT count(*) FROM orders')" "30000"
chk "remote does not have it" "$(r 'SELECT count(*) FROM orders WHERE id=40000')" "0"

echo; echo "===== divergence: remote stayed at 30000 throughout ====="
chk "remote total unchanged" "$(r 'SELECT count(*) FROM orders')" "30000"

echo
[ "$ok" = 1 ] && echo "✅ OVERLAY PoC PASSED: correct on all cases that broke the partition approach." \
              || { echo "❌ OVERLAY PoC FAILED"; exit 1; }
