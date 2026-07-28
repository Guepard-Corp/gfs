#!/usr/bin/env bash
# Local Docker E2E for gfs clone (planner-hook model).
# 1. Mock "prod" Postgres with seed data
# 2. gfs clone into a local repo (requires gfs-postgres:16 image with gfs extension)
# 3. Assert reads, federation, and local writes diverge from prod
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
GFS_BIN="${GFS_BIN:-$REPO_ROOT/target/debug/gfs}"
GFS_IMAGE="${GFS_IMAGE:-gfs-postgres:16}"
REMOTE_NAME="${REMOTE_NAME:-gfs-mock-prod}"
REMOTE_PORT="${REMOTE_PORT:-55450}"
CLONE_PORT="${CLONE_PORT:-55451}"
CLONE_DIR="${CLONE_DIR:-$REPO_ROOT/.gfs-e2e-clone-local}"
# Linux: bridge gateway. macOS Docker Desktop: host.docker.internal
DOCKER_HOST_IP="${DOCKER_HOST_IP:-172.17.0.1}"

log() { printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*"; }
pass() { log "PASS: $*"; }
fail() { log "FAIL: $*"; exit 1; }

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "missing command: $1"
}

cleanup() {
  log "cleanup"
  docker rm -f "$REMOTE_NAME" 2>/dev/null || true
  local cid
  cid="$(docker ps -q --filter "publish=${CLONE_PORT}" 2>/dev/null | head -1 || true)"
  [[ -n "$cid" ]] && docker rm -f "$cid" 2>/dev/null || true
}

trap cleanup EXIT INT TERM

require_cmd docker
[[ -x "$GFS_BIN" ]] || fail "gfs binary not found at $GFS_BIN (run: cargo build -p gfs-cli)"
docker image inspect "$GFS_IMAGE" >/dev/null 2>&1 \
  || fail "image $GFS_IMAGE missing (run: docker build -t $GFS_IMAGE $REPO_ROOT/crates/extensions/gfs)"

# --- mock prod ---
log "starting mock prod postgres on :$REMOTE_PORT"
docker rm -f "$REMOTE_NAME" 2>/dev/null || true
docker run -d --name "$REMOTE_NAME" \
  -e POSTGRES_PASSWORD=postgres \
  -e POSTGRES_DB=shop \
  -p "${REMOTE_PORT}:5432" \
  postgres:16 >/dev/null

for _ in $(seq 1 90); do
  if docker exec "$REMOTE_NAME" pg_isready -U postgres -d shop >/dev/null 2>&1 \
    && docker exec "$REMOTE_NAME" psql -U postgres -d shop -tAc 'SELECT 1' >/dev/null 2>&1; then
    break
  fi
  sleep 1
done
docker exec "$REMOTE_NAME" pg_isready -U postgres -d shop
sleep 1

log "seeding mock prod data"
docker exec "$REMOTE_NAME" psql -U postgres -d shop -v ON_ERROR_STOP=1 -c "
  DO \$\$ BEGIN
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'gfs_reader') THEN
      CREATE ROLE gfs_reader LOGIN PASSWORD 'readerpw';
    END IF;
  END \$\$;

  CREATE TABLE IF NOT EXISTS customers (
    id bigint PRIMARY KEY,
    name text NOT NULL
  );
  CREATE TABLE IF NOT EXISTS orders (
    id bigint PRIMARY KEY,
    customer_id bigint NOT NULL REFERENCES customers(id),
    amount numeric(10,2) NOT NULL,
    status text NOT NULL DEFAULT 'open'
  );

  TRUNCATE orders, customers CASCADE;
  INSERT INTO customers (id, name)
  SELECT g, 'customer_' || g FROM generate_series(1, 100) g;
  INSERT INTO orders (id, customer_id, amount, status)
  SELECT g, (g % 100) + 1, (g % 500) + 0.5, 'open'
  FROM generate_series(1, 5000) g;

  GRANT USAGE ON SCHEMA public TO gfs_reader;
  GRANT SELECT ON ALL TABLES IN SCHEMA public TO gfs_reader;
" >/dev/null

remote_orders="$(docker exec "$REMOTE_NAME" psql -U postgres -d shop -tAc 'SELECT count(*) FROM orders')"
remote_customers="$(docker exec "$REMOTE_NAME" psql -U postgres -d shop -tAc 'SELECT count(*) FROM customers')"
log "mock prod: customers=$remote_customers orders=$remote_orders"
[[ "$remote_orders" == "5000" ]] || fail "unexpected remote order count: $remote_orders"

# --- gfs clone ---
rm -rf "$CLONE_DIR"
mkdir -p "$CLONE_DIR"

clone_url="postgres://gfs_reader:readerpw@${DOCKER_HOST_IP}:${REMOTE_PORT}/shop"
log "gfs clone from $clone_url (image=$GFS_IMAGE port=$CLONE_PORT)"
if ! "$GFS_BIN" clone \
  --from "$clone_url" \
  --image "$GFS_IMAGE" \
  --port "$CLONE_PORT" \
  "$CLONE_DIR"; then
  fail "gfs clone command failed"
fi

gfs_cid="$(docker ps -q --filter "publish=${CLONE_PORT}" | head -1)"
[[ -n "$gfs_cid" ]] || fail "no gfs postgres container on port $CLONE_PORT"

gfs_psql() {
  docker exec "$gfs_cid" psql -U postgres -d postgres -tAc "$1" 2>/dev/null | tr -d '\r'
}

local_live_tup() {
  gfs_psql "SELECT n_live_tup::text FROM pg_stat_user_tables WHERE relname='$1'"
}

# --- bootstrap checks (planner-hook model) ---
[[ "$(gfs_psql "SELECT extname FROM pg_extension WHERE extname='gfs'")" == "gfs" ]] \
  || fail "gfs extension not installed"
[[ "$(gfs_psql "SELECT relkind FROM pg_class WHERE relname='orders' AND relnamespace='public'::regnamespace")" == "r" ]] \
  || fail "orders should be a real heap table (relkind=r)"
[[ "$(gfs_psql "SELECT count(*) FROM gfs.clone_source WHERE relid='public.orders'::regclass")" == "1" ]] \
  || fail "orders not registered in gfs.clone_source"
[[ "$(local_live_tup orders)" == "0" ]] \
  || fail "local orders heap should start empty (n_live_tup=0)"
pass "bootstrap: extension, heap table, clone registry, zero local rows"

# --- read: point lookup hydrates ---
[[ "$(gfs_psql 'SELECT amount = 42.5 FROM orders WHERE id = 42')" == "t" ]] \
  || fail "point read id=42 (got amount=$(gfs_psql 'SELECT amount::text FROM orders WHERE id = 42'))"
live_after_point="$(local_live_tup orders)"
[[ "$live_after_point" != "0" ]] \
  || fail "point read should hydrate at least one local row (n_live_tup=$live_after_point)"
pass "point read hydrates single row"

# --- read: live remote changes on still-unhydrated rows (before count*) ---
docker exec "$REMOTE_NAME" psql -U postgres -d shop -c \
  "UPDATE orders SET status='shipped' WHERE id = 100" >/dev/null
[[ "$(gfs_psql "SELECT status FROM orders WHERE id = 100")" == "shipped" ]] \
  || fail "live remote update on unhydrated row id=100"
pass "unhydrated read reflects live remote"

# --- read: count federates then may whole-hydrate affordable tables ---
clone_count="$(gfs_psql 'SELECT count(*) FROM orders')"
[[ "$clone_count" == "$remote_orders" ]] \
  || fail "count(*) expected $remote_orders got $clone_count"
pass "count(*) matches remote ($clone_count rows)"

# After count(*), affordable tables may be fully materialized locally (whole_cached).
# Remote changes to already-materialized rows are NOT propagated — by design.
docker exec "$REMOTE_NAME" psql -U postgres -d shop -c \
  "UPDATE orders SET status='remote_after_materialize' WHERE id = 200" >/dev/null
if [[ "$(gfs_psql "SELECT whole_cached FROM gfs.clone_source WHERE relid='public.orders'::regclass")" == "t" ]]; then
  [[ "$(gfs_psql "SELECT status FROM orders WHERE id = 200")" != "remote_after_materialize" ]] \
    && pass "materialized rows stay frozen after remote changes (expected)"
fi

# --- write: local diverges, remote unchanged ---
gfs_psql "UPDATE orders SET status='local_only' WHERE id = 42" >/dev/null
[[ "$(gfs_psql "SELECT status FROM orders WHERE id = 42")" == "local_only" ]] \
  || fail "local update not visible"
[[ "$(docker exec "$REMOTE_NAME" psql -U postgres -d shop -tAc "SELECT status FROM orders WHERE id = 42")" == "open" ]] \
  || fail "remote row 42 should be unchanged"
gfs_psql "INSERT INTO orders (id, customer_id, amount, status) VALUES (99999, 1, 9.99, 'clone_only')" >/dev/null
[[ "$(gfs_psql 'SELECT count(*) FROM orders WHERE id = 99999')" == "1" ]] \
  || fail "local-only insert missing"
[[ "$(docker exec "$REMOTE_NAME" psql -U postgres -d shop -tAc 'SELECT count(*) FROM orders WHERE id = 99999')" == "0" ]] \
  || fail "local-only row leaked to remote"
pass "writes stay local (copy-on-write)"

# --- optional: warm whole table (no-op if already whole_cached from count) ---
if [[ "$(gfs_psql "SELECT whole_cached FROM gfs.clone_source WHERE relid='public.orders'::regclass")" != "t" ]]; then
  warmed="$(gfs_psql "SELECT gfs.warm('public.orders'::regclass)")"
  log "gfs.warm copied $warmed rows"
  [[ "$(local_live_tup orders)" == "$remote_orders" ]] \
    || fail "after warm, local n_live_tup should match remote ($remote_orders)"
  pass "gfs.warm fully materializes table"
else
  pass "table already whole_cached after count(*) — skipping gfs.warm"
fi

log "========================================"
log "ALL TESTS PASSED"
log "  mock prod:  docker exec -it $REMOTE_NAME psql -U postgres -d shop"
log "  clone repo: $CLONE_DIR"
log "  clone db:   docker exec -it $gfs_cid psql -U postgres -d postgres"
log "  host port:  localhost:$CLONE_PORT"
log "========================================"

if [[ "${GFS_E2E_CLEANUP:-0}" == "1" ]]; then
  cleanup
else
  trap - EXIT INT TERM
  log "containers left running (set GFS_E2E_CLEANUP=1 to auto-remove)"
fi
