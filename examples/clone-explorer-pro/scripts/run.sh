#!/usr/bin/env bash
# Bring up a large multi-table SOURCE and serve the richer SOURCE-vs-CLONE
# explorer (joins, fuzzy search, temporal filters, aggregate dashboard, writes).
# The clone is created from the UI. `--proxy` fronts the clone with the
# auto-warming proxy binary, built and run on the host (rebuilt every run).
set -euo pipefail

cd "$(dirname "$0")/.."
APP_DIR="$(pwd)"
REPO_ROOT="$(cd ../.. && pwd)"
[[ -f .env ]] && set -a && . ./.env && set +a

GFS_BIN="${GFS_BIN:-$REPO_ROOT/target/debug/gfs}"
SEED_PRODUCTS="${SEED_PRODUCTS:-1000000}"
SEED_CUSTOMERS="${SEED_CUSTOMERS:-50000}"
SEED_ORDERS="${SEED_ORDERS:-300000}"
SEED_REVIEWS="${SEED_REVIEWS:-250000}"
SEED_EVENTS="${SEED_EVENTS:-1000000}"
SOURCE_PORT="${SOURCE_PORT:-55452}"
CLONE_PORT="${CLONE_PORT:-55453}"
PROXY_PORT="${PROXY_PORT:-55454}"
PROXY_METRICS_PORT="${PROXY_METRICS_PORT:-9091}"
SERVER_PORT="${SERVER_PORT:-8788}"
REMOTE_HOST="${REMOTE_HOST:-host.docker.internal}"
CLONE_DIR="$APP_DIR/clone-repo"
SOURCE="gfs-explorer-pro-source"
PROXY_MODE=""; [[ "${1:-}" == "--proxy" ]] && PROXY_MODE=1

step() { printf '\n\033[1;34m== %s ==\033[0m\n' "$1"; }

if [[ ! -x "$GFS_BIN" ]]; then
  echo "gfs binary not found at $GFS_BIN  (build: cd $REPO_ROOT && cargo build -p gfs-cli)"
  exit 1
fi

step "Install Node dependencies"
[[ -d node_modules ]] || pnpm install

step "Build web UI"
pnpm --filter clone-explorer-pro-web run build

step "Start source PostgreSQL 16"
SOURCE_PORT="$SOURCE_PORT" docker compose up -d source
echo -n "  waiting for source"
until [[ "$(docker inspect -f '{{.State.Health.Status}}' "$SOURCE" 2>/dev/null)" == "healthy" ]]; do echo -n .; sleep 1; done
echo " ok"

step "Apply schema (source)"
docker exec -i "$SOURCE" psql -U app -d appdb -v ON_ERROR_STOP=1 -f - < sql/00-extensions.sql
docker exec -i "$SOURCE" psql -U app -d appdb -v ON_ERROR_STOP=1 -f - < sql/01-schema.sql

step "Seed (products=$SEED_PRODUCTS customers=$SEED_CUSTOMERS orders=$SEED_ORDERS reviews=$SEED_REVIEWS events=$SEED_EVENTS)"
docker exec -i "$SOURCE" psql -U app -d appdb -v ON_ERROR_STOP=1 \
  -v products="$SEED_PRODUCTS" -v customers="$SEED_CUSTOMERS" -v orders="$SEED_ORDERS" \
  -v reviews="$SEED_REVIEWS" -v events="$SEED_EVENTS" -f - < sql/02-seed.sql
docker exec "$SOURCE" psql -U app -d appdb -tAc \
  "SELECT 'products '||(SELECT count(*) FROM products)||' · orders '||(SELECT count(*) FROM orders)||' · reviews '||(SELECT count(*) FROM reviews)"

step "Clear any previous clone (created fresh from the UI)"
OLD_CLONE="$(docker ps -q --filter "publish=${CLONE_PORT}")"
[[ -n "$OLD_CLONE" ]] && docker rm -f "$OLD_CLONE" >/dev/null
rm -rf "$CLONE_DIR"

if [[ -n "$PROXY_MODE" ]]; then
  step "Build + start the guepard proxy binary (in front of the clone)"
  # Drop a proxy container left over from an older dockerized run (it would hold
  # PROXY_PORT and the binary would die with "address already in use").
  docker rm -f gfs-explorer-pro-proxy >/dev/null 2>&1 || true
  # Build fresh each run (override PROXY_BIN with a prebuilt binary to skip it).
  if [[ -n "${PROXY_BIN:-}" ]]; then
    PROXY_RUN="$PROXY_BIN"
  else
    ( cd "$REPO_ROOT" && cargo build -p guepard-proxy-v2 )
    PROXY_RUN="$REPO_ROOT/target/debug/guepard-proxy-v2"
  fi
  # Auto-discovery: no --backend. The proxy watches Docker for the clone the UI
  # creates (labels gfs.role=clone/gfs.provider=postgres) and fronts it on its
  # own listener. --listen-base = PROXY_PORT so this single clone lands exactly on
  # PROXY_PORT, keeping CLONE_URL stable. Live map at /clones on the metrics port.
  "$PROXY_RUN" \
    --discover --listen-base "${PROXY_PORT}" \
    --metrics "127.0.0.1:${PROXY_METRICS_PORT}" \
    --warm --warm-dbname postgres \
    --refresh-interval 3 \
    --cache-metrics --cache-metrics-interval 2 &
  PROXY_PID=$!
  trap 'kill "$PROXY_PID" 2>/dev/null || true' EXIT INT TERM
  # A background crash (e.g. port in use) is silent under set -e — surface it.
  sleep 0.5
  if ! kill -0 "$PROXY_PID" 2>/dev/null; then
    echo "proxy failed to start (port ${PROXY_PORT}/${PROXY_METRICS_PORT} in use?). See the error above."
    exit 1
  fi
  echo "  proxy[pid $PROXY_PID] auto-discovering clones → first lands on localhost:${PROXY_PORT}; metrics+/clones :${PROXY_METRICS_PORT}"
  CLONE_URL="postgres://postgres:postgres@localhost:${PROXY_PORT}/postgres"
else
  CLONE_URL="postgres://postgres:postgres@localhost:${CLONE_PORT}/postgres"
fi

step "Serve the explorer"
cat <<EOF

  Open http://localhost:${SERVER_PORT}   (click "Clone the source" on the right)

  Scenarios to try on the CLONE:
    * Browse products by id range        -> elided once warmed (key predicate)
    * Fuzzy search products / reviews     -> federates, then local once whole-cached
    * Filter by category                  -> JOIN products⋈categories (overlays)
    * Customer order history              -> 3-table JOIN
    * Recent orders (last N days)         -> temporal filter (federates)
    * Dashboard (revenue / category)      -> aggregate JOIN (federates: anti-join blocks push-down)
    * Place order / write review          -> copy-on-write divergence; source untouched
$([[ -n "$PROXY_MODE" ]] && echo "    * PROXY MODE: just browse — pages flip remote->local on their own.")

  Tear down: docker compose down -v ; rm -rf "$CLONE_DIR"

EOF

SOURCE_URL="postgres://app:app@localhost:${SOURCE_PORT}/appdb" \
CLONE_URL="$CLONE_URL" PROXY_MODE="$PROXY_MODE" \
SERVER_PORT="$SERVER_PORT" GFS_BIN="$GFS_BIN" CLONE_DIR="$CLONE_DIR" CLONE_PORT="$CLONE_PORT" \
REMOTE_HOST="$REMOTE_HOST" SOURCE_PORT="$SOURCE_PORT" \
SOURCE_DB="appdb" SOURCE_USER="app" SOURCE_PASS="app" DB_VERSION="16" \
  pnpm --filter clone-explorer-pro-server run start
