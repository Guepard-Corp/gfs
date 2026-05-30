#!/usr/bin/env bash
# Bring up a large SOURCE database and serve a web app that shows the SOURCE and
# its GFS CLONE side by side. The clone itself is triggered by the user from the
# UI (so the clone, and its timing, are part of the demo).
#
#   1. start source PostgreSQL 16 + apply extensions/schema
#   2. seed SEED_ROWS products on the source (the clone copies nothing up front)
#   3. build the web UI and start the server pointing at the source
#   -> open the UI and click "Clone the source"
#
# Requires: Docker Desktop, Node 20+, and a built `gfs` binary.
set -euo pipefail

cd "$(dirname "$0")/.."          # examples/clone-explorer
APP_DIR="$(pwd)"
REPO_ROOT="$(cd ../.. && pwd)"

# Load .env if present (SEED_ROWS, ports, REMOTE_HOST).
[[ -f .env ]] && set -a && . ./.env && set +a

GFS_BIN="${GFS_BIN:-$REPO_ROOT/target/debug/gfs}"
SEED_ROWS="${SEED_ROWS:-500000}"
SOURCE_PORT="${SOURCE_PORT:-55442}"
CLONE_PORT="${CLONE_PORT:-55443}"
SERVER_PORT="${SERVER_PORT:-8787}"
REMOTE_HOST="${REMOTE_HOST:-host.docker.internal}"
CLONE_DIR="$APP_DIR/clone-repo"
SOURCE="gfs-explorer-source"
PROXY_PORT="${PROXY_PORT:-55444}"
PROXY_METRICS_PORT="${PROXY_METRICS_PORT:-9090}"
# `run.sh --proxy` routes the clone through the guepard proxy binary (built and
# run on the host), which auto-warms reads (no manual "warm" button needed).
PROXY_MODE=""; [[ "${1:-}" == "--proxy" ]] && PROXY_MODE=1

step() { printf '\n\033[1;34m== %s ==\033[0m\n' "$1"; }

if [[ ! -x "$GFS_BIN" ]]; then
  echo "gfs binary not found at $GFS_BIN"
  echo "Build it first:  (cd $REPO_ROOT && cargo build -p gfs-cli)"
  echo "or set GFS_BIN=/path/to/gfs"
  exit 1
fi

step "Install Node dependencies"
[[ -d node_modules ]] || pnpm install

step "Build web UI"
pnpm --filter clone-explorer-web run build

step "Start source PostgreSQL 16"
SOURCE_PORT="$SOURCE_PORT" docker compose up -d
echo -n "  waiting for source to be healthy"
until [[ "$(docker inspect -f '{{.State.Health.Status}}' "$SOURCE" 2>/dev/null)" == "healthy" ]]; do
  echo -n "."; sleep 1
done
echo " ok"

step "Apply extensions + schema (source)"
docker exec -i "$SOURCE" psql -U app -d appdb -v ON_ERROR_STOP=1 -f - < sql/00-extensions.sql
docker exec -i "$SOURCE" psql -U app -d appdb -v ON_ERROR_STOP=1 -f - < sql/01-schema.sql

step "Seed $SEED_ROWS products (source)"
docker exec -i "$SOURCE" psql -U app -d appdb -v ON_ERROR_STOP=1 -v rows="$SEED_ROWS" -f - < sql/02-seed.sql
docker exec "$SOURCE" psql -U app -d appdb -tAc "SELECT 'source rows: ' || count(*) FROM products"

step "Clear any previous clone (the UI will create a fresh one)"
OLD_CLONE="$(docker ps -q --filter "publish=${CLONE_PORT}")"
[[ -n "$OLD_CLONE" ]] && docker rm -f "$OLD_CLONE" >/dev/null
rm -rf "$CLONE_DIR"

if [[ -n "$PROXY_MODE" ]]; then
  step "Build + start the guepard proxy binary (in front of the clone)"
  # Remove a proxy container left over from an older dockerized run — it would
  # still hold PROXY_PORT and the binary would die with "address already in use".
  docker rm -f gfs-explorer-proxy >/dev/null 2>&1 || true
  # Build fresh each run so proxy source changes are picked up (incremental, fast).
  # Override PROXY_BIN with a prebuilt/release binary to skip the build.
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

  Open http://localhost:${SERVER_PORT}

  Left  = SOURCE  (postgres://app:app@localhost:${SOURCE_PORT}/appdb)
  Right = CLONE   ->  click "Clone the source" in the UI (it is timed)

  Then try:
    * page through both: the clone shows "remote read", the source "source"
    * click "Warm this page" on the clone -> badge flips to "local (elided)"
    * edit a price / place an order on the clone -> the source stays unchanged
    * edit a price on the source -> a COLD clone row reflects it; a WARMED one does not

  The source must stay up while you query the clone (copy-on-read).
  Tear down:  docker compose down -v ; rm -rf "$CLONE_DIR"

EOF

[[ -n "$PROXY_MODE" ]] && echo "  PROXY MODE: just browse the clone — the proxy auto-warms; pages flip remote→local on their own."

SOURCE_URL="postgres://app:app@localhost:${SOURCE_PORT}/appdb" \
CLONE_URL="$CLONE_URL" \
PROXY_MODE="$PROXY_MODE" \
SERVER_PORT="$SERVER_PORT" \
GFS_BIN="$GFS_BIN" \
CLONE_DIR="$CLONE_DIR" \
CLONE_PORT="$CLONE_PORT" \
REMOTE_HOST="$REMOTE_HOST" \
SOURCE_PORT="$SOURCE_PORT" \
SOURCE_DB="appdb" SOURCE_USER="app" SOURCE_PASS="app" DB_VERSION="16" \
  pnpm --filter clone-explorer-server run start
