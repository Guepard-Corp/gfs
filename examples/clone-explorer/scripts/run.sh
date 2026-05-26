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

SOURCE_URL="postgres://app:app@localhost:${SOURCE_PORT}/appdb" \
CLONE_URL="postgres://postgres:postgres@localhost:${CLONE_PORT}/postgres" \
SERVER_PORT="$SERVER_PORT" \
GFS_BIN="$GFS_BIN" \
CLONE_DIR="$CLONE_DIR" \
CLONE_PORT="$CLONE_PORT" \
REMOTE_HOST="$REMOTE_HOST" \
SOURCE_PORT="$SOURCE_PORT" \
SOURCE_DB="appdb" SOURCE_USER="app" SOURCE_PASS="app" DB_VERSION="16" \
  pnpm --filter clone-explorer-server run start
