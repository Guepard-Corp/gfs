#!/usr/bin/env bash
# Bring up a PERSISTENT TPC-H source (shared lib-source.sh) and serve the benchmark
# EXPLORER (UI). The clone is created from the UI (copy-on-read via the gfs
# planner-hook extension). For the headless front-end of the SAME benchmark, use
# scripts/bench.sh.
#
#   SF=1 ./scripts/run.sh        # small (proof); SF=10 ~16GB, SF=50 ~100GB
# The source persists across runs (named volume per SF) -- only the lazy clone is
# rebuilt. Override REBUILD_SOURCE=1 to regenerate, DROP_SOURCE=1 to delete it.
set -euo pipefail
cd "$(dirname "$0")/.."
APP_DIR="$(pwd)"
REPO_ROOT="$(cd ../.. && pwd)"
[[ -f .env ]] && set -a && . ./.env && set +a

SF="${SF:-1}"; SFTAG="${SF//./p}"
GFS_BIN="${GFS_BIN:-$REPO_ROOT/target/debug/gfs}"
GFS_IMAGE="${GFS_IMAGE:-gfs-postgres:16}"
PSQL="${PSQL:-/opt/homebrew/opt/postgresql@16/bin/psql}"
SOURCE_PORT="${SOURCE_PORT:-55620}"
CLONE_PORT="${CLONE_PORT:-55621}"
SERVER_PORT="${SERVER_PORT:-8789}"
REMOTE_HOST="${REMOTE_HOST:-host.docker.internal}"
CLONE_DIR="$APP_DIR/clone-repo"
SRC_NAME="gfs-bx-tpch-src-sf${SFTAG}"
SRC_VOL="gfs-bx-tpch-vol-sf${SFTAG}"
DATA_DIR="${DATA_DIR:-/tmp/gfs-bx-tpch-sf${SFTAG}}"

. "$(dirname "$0")/lib-source.sh"
ensure_source

step "Clear any previous clone (created fresh from the UI)"
OLD="$(docker ps -q --filter "publish=${CLONE_PORT}")"; [[ -n "$OLD" ]] && docker rm -f "$OLD" >/dev/null
rm -rf "$CLONE_DIR"

step "Install Node dependencies"; [[ -d node_modules ]] || pnpm install
step "Build web UI"; pnpm --filter benchmark-explorer-web run build

step "Serve the explorer"
cat <<EOF

  Open http://localhost:${SERVER_PORT}   (click "Clone the source")

  Try, on the clone, watching the route badge + the Router-state panel:
    * Range scan (P1)        -> fetched (the key range) then local (re-run = elision)
    * Selective filter (P2)  -> federated, then fetched (the slice), then local
    * Q1/Q3/Q5/Q10 (P3)      -> federated (the join is pushed to the source)
    * "plan"                 -> EXPLAIN shows a single Foreign Scan = pushed-down join
    * Cost weights           -> lower the ceiling, re-run, watch routing change
    * "reset clone"          -> replay the paths from a cold clone

  Headless (same scenarios, no UI):  SF=$SF ./scripts/bench.sh
  Tear down: DROP_SOURCE=1 SF=$SF ./scripts/run.sh ; rm -rf "$CLONE_DIR"

EOF

SOURCE_URL="postgres://app:pw@localhost:${SOURCE_PORT}/tpch" \
CLONE_URL="postgres://postgres:postgres@localhost:${CLONE_PORT}/postgres" \
SERVER_PORT="$SERVER_PORT" GFS_BIN="$GFS_BIN" CLONE_DIR="$CLONE_DIR" CLONE_PORT="$CLONE_PORT" \
REMOTE_HOST="$REMOTE_HOST" SOURCE_PORT="$SOURCE_PORT" \
SOURCE_DB="tpch" SOURCE_USER="app" SOURCE_PASS="pw" DB_VERSION="16" CLONE_IMAGE="$GFS_IMAGE" \
  pnpm --filter benchmark-explorer-server run start
