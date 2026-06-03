#!/usr/bin/env bash
# End-to-end test of TRANSITIVE FK WARMING (RFC 008, "level 0").
#
# The overlay clone reads through to the remote on demand. A JOIN, however, is
# the overlay's weak spot: the overlay view (local UNION ALL foreign - anti-join)
# defeats postgres_fdw's join pushdown, so a cold join federates BOTH sides
# (a Foreign Scan per table). This demo proves the fix: warming the queried
# CHILD table follows its foreign keys to warm the referenced PARENT rows (and
# promotes small parents to whole_table), so the same join becomes 100% local —
# zero Foreign Scan — with NO app change and NO custom extension.
#
# It runs the REAL `gfs clone` against a REAL source, then asserts on the clone's
# EXPLAIN plans. Deterministic PASS/FAIL. Self-contained: starts/stops its own
# containers and removes the clone repo on exit.
#
# Requires: Docker Desktop, and a built `gfs` binary
#   (cargo build -p gfs-cli ; or set GFS_BIN=/path/to/gfs).
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
GFS_BIN="${GFS_BIN:-$REPO_ROOT/target/debug/gfs}"

SOURCE="gfs-joins-source"
SOURCE_PORT="${SOURCE_PORT:-55462}"
CLONE_DIR="$HERE/joins-clone"
CLONE_PORT="${CLONE_PORT:-55463}"
REMOTE_HOST="${REMOTE_HOST:-host.docker.internal}"
IMG="postgres:16"

PASS=0; FAIL=0
ok()   { printf '  \033[1;32mPASS\033[0m %s\n' "$1"; PASS=$((PASS+1)); }
bad()  { printf '  \033[1;31mFAIL\033[0m %s\n' "$1"; FAIL=$((FAIL+1)); }
step() { printf '\n\033[1;34m== %s ==\033[0m\n' "$1"; }

cleanup() {
  step "Tear down"
  # Stop the gfs-managed clone via the repo (its container name is gfs-chosen),
  # then drop the source and any container still holding the clone port.
  if [[ -d "$CLONE_DIR" ]]; then ( cd "$CLONE_DIR" && "$GFS_BIN" compute stop >/dev/null 2>&1 ); fi
  local clone_c; clone_c="$(docker ps -aq --filter "publish=${CLONE_PORT}")"
  [[ -n "$clone_c" ]] && docker rm -f "$clone_c" >/dev/null 2>&1
  docker rm -f "$SOURCE" >/dev/null 2>&1
  rm -rf "$CLONE_DIR"
}
trap cleanup EXIT

if [[ ! -x "$GFS_BIN" ]]; then
  echo "gfs binary not found at $GFS_BIN"
  echo "Build it:  (cd $REPO_ROOT && cargo build -p gfs-cli)   or set GFS_BIN=/path/to/gfs"
  exit 1
fi

# psql helpers — connect over the PUBLISHED host ports so we never depend on the
# clone's container name (gfs picks it). Needs a local psql client; if absent we
# fall back to running psql inside the source container against host.docker.internal.
if command -v psql >/dev/null 2>&1; then
  ssql() { PGPASSWORD=postgres psql -h localhost -p "$SOURCE_PORT" -U postgres -d shop     -v ON_ERROR_STOP=1 -tAc "$1"; }
  csql() { PGPASSWORD=postgres psql -h localhost -p "$CLONE_PORT"  -U postgres -d postgres -tAc "$1"; }
else
  ssql() { docker exec -i "$SOURCE" psql -U postgres -d shop -v ON_ERROR_STOP=1 -tAc "$1"; }
  csql() { docker exec -i "$SOURCE" psql -U postgres -d postgres -h "$REMOTE_HOST" -p "$CLONE_PORT" -tAc "$1"; }
fi

step "Start source PostgreSQL ($IMG) on :$SOURCE_PORT"
docker rm -f "$SOURCE" >/dev/null 2>&1
docker run -d --name "$SOURCE" -e POSTGRES_PASSWORD=postgres -e POSTGRES_DB=shop \
  -p "127.0.0.1:${SOURCE_PORT}:5432" "$IMG" >/dev/null
for _ in $(seq 1 60); do docker exec "$SOURCE" pg_isready -U postgres -d shop >/dev/null 2>&1 && break; sleep 0.5; done

step "Seed a star schema with foreign keys (customers <- orders -> products)"
ssql "
  CREATE TABLE customers (id bigint PRIMARY KEY, name text NOT NULL);
  CREATE TABLE products  (id bigint PRIMARY KEY, name text NOT NULL, price numeric NOT NULL);
  CREATE TABLE orders (
    id          bigint PRIMARY KEY,
    customer_id bigint NOT NULL REFERENCES customers(id),
    product_id  bigint NOT NULL REFERENCES products(id),
    qty         int NOT NULL
  );
  INSERT INTO customers SELECT g, 'cust'||g  FROM generate_series(1,2000)  g;
  INSERT INTO products  SELECT g, 'prod'||g, (g%100)+1.5 FROM generate_series(1,2000) g;
  INSERT INTO orders SELECT g, ((g-1)%2000)+1, ((g*7-1)%2000)+1, (g%5)+1 FROM generate_series(1,20000) g;
" >/dev/null
echo "  source: $(ssql 'SELECT count(*) FROM orders') orders, $(ssql 'SELECT count(*) FROM customers') customers, $(ssql 'SELECT count(*) FROM products') products"

step "gfs clone the source (copies nothing up front)"
rm -rf "$CLONE_DIR"
URL="postgres://postgres:postgres@${REMOTE_HOST}:${SOURCE_PORT}/shop"
if ! "$GFS_BIN" clone --from "$URL" --database-version 16 --port "$CLONE_PORT" "$CLONE_DIR"; then
  echo "clone failed"; exit 1
fi
echo "  clone ready on localhost:$CLONE_PORT (overlay views in db 'postgres')"

# The join we care about: a customer's orders joined to the product catalog,
# scoped to a contiguous key range so warming the child is range-elidable.
JOIN="SELECT o.id, c.name AS customer, p.name AS product, o.qty
        FROM orders o
        JOIN customers c ON c.id = o.customer_id
        JOIN products  p ON p.id = o.product_id
       WHERE o.id BETWEEN 1 AND 200"

step "1) COLD join — expect it to federate (Foreign Scan present)"
COLD_FS=$(csql "EXPLAIN (VERBOSE) $JOIN" | grep -c "Foreign Scan")
echo "  Foreign Scan nodes (cold): $COLD_FS"
[[ "$COLD_FS" -ge 1 ]] && ok "cold join federates to the remote (the problem)" \
                        || bad "expected the cold join to federate"

step "2) Warm the queried child range — transitive FK warming follows the FKs"
# warm_query_chunks hydrates the orders chunk AND, via the FKs, the referenced
# customers/products rows, promoting those small dimension tables to whole_table.
csql "SELECT gfs_sync.warm_query_chunks('SELECT * FROM orders WHERE id BETWEEN 1 AND 200', 1000)" >/dev/null
csql "SELECT gfs_sync.refresh_exclusions()" >/dev/null
echo "  local rows now: orders=$(csql 'SELECT count(*) FROM public.orders')" \
     "customers=$(csql 'SELECT count(*) FROM public.customers')" \
     "products=$(csql 'SELECT count(*) FROM public.products')"

PAR_C=$(csql "SELECT count(*) FROM public.customers")
PAR_P=$(csql "SELECT count(*) FROM public.products")
{ [[ "$PAR_C" -gt 0 ]] && [[ "$PAR_P" -gt 0 ]]; } \
  && ok "transitive FK warm hydrated BOTH parent tables (customers + products)" \
  || bad "parent tables were not warmed transitively (customers=$PAR_C products=$PAR_P)"

step "3) WARM join — expect it fully local (zero Foreign Scan)"
WARM_FS=$(csql "EXPLAIN (VERBOSE) $JOIN" | grep -c "Foreign Scan")
echo "  Foreign Scan nodes (warm): $WARM_FS"
[[ "$WARM_FS" -eq 0 ]] && ok "the join is now served entirely locally (no remote contact)" \
                        || bad "join still federates ($WARM_FS Foreign Scan) after transitive warm"

step "4) Correctness — clone join result equals the source"
C_RES=$(csql "SELECT count(*), coalesce(sum(o.qty),0)
                FROM orders o JOIN customers c ON c.id=o.customer_id
                JOIN products p ON p.id=o.product_id WHERE o.id BETWEEN 1 AND 200")
S_RES=$(ssql "SELECT count(*), coalesce(sum(o.qty),0)
                FROM orders o JOIN customers c ON c.id=o.customer_id
                JOIN products p ON p.id=o.product_id WHERE o.id BETWEEN 1 AND 200")
echo "  clone=$C_RES  source=$S_RES"
[[ "$C_RES" == "$S_RES" ]] && ok "join result identical to source" \
                            || bad "join result diverged from source"

step "5) The source stayed read-only (copy-on-read, no writes pushed upstream)"
S_ORDERS=$(ssql "SELECT count(*) FROM orders")
[[ "$S_ORDERS" == "20000" ]] && ok "source unchanged (20000 orders)" \
                              || bad "source row count changed ($S_ORDERS)"

printf '\n\033[1m================ RESULT: %d passed, %d failed ================\033[0m\n' "$PASS" "$FAIL"
[[ "$FAIL" -eq 0 ]]
