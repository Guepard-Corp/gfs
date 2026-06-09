#!/usr/bin/env bash
# HEADLESS front-end of the benchmark explorer — the SAME scenarios the UI runs
# (server/src/bench.ts), with no browser. Brings up the persistent TPC-H source
# (shared lib-source.sh), provisions a fresh lazy clone, replays the router paths
# on the clone AND the source asserting clone == source + the expected route per
# shot, proves convergence (a federated join goes fully local after warming), and
# writes objective metrics to benchmark.results.tsv with a regression compare.
# Exit 0 on all-pass, 1 otherwise.
#
#   SF=1 ./scripts/bench.sh                 # fast proof (~1 GB)
#   LABEL=my-run SF=1 ./scripts/bench.sh    # label the results row
#   SF=10 ./scripts/bench.sh                # ~16 GB
# The source persists across runs (named volume per SF); only the clone is rebuilt.
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
REMOTE_HOST="${REMOTE_HOST:-host.docker.internal}"
CLONE_DIR="$APP_DIR/clone-repo"
SRC_NAME="gfs-bx-tpch-src-sf${SFTAG}"
SRC_VOL="gfs-bx-tpch-vol-sf${SFTAG}"
DATA_DIR="${DATA_DIR:-/tmp/gfs-bx-tpch-sf${SFTAG}}"

. "$(dirname "$0")/lib-source.sh"
ensure_source

step "Clear any previous clone"
OLD="$(docker ps -q --filter "publish=${CLONE_PORT}")"; [[ -n "$OLD" ]] && docker rm -f "$OLD" >/dev/null
rm -rf "$CLONE_DIR"

step "Install Node dependencies"; [[ -d node_modules ]] || pnpm install

step "Run the headless benchmark (same scenarios as the UI)"
SOURCE_URL="postgres://app:pw@localhost:${SOURCE_PORT}/tpch" \
CLONE_URL="postgres://postgres:postgres@localhost:${CLONE_PORT}/postgres" \
GFS_BIN="$GFS_BIN" CLONE_DIR="$CLONE_DIR" CLONE_PORT="$CLONE_PORT" \
REMOTE_HOST="$REMOTE_HOST" SOURCE_PORT="$SOURCE_PORT" \
SOURCE_DB="tpch" SOURCE_USER="app" SOURCE_PASS="pw" DB_VERSION="16" CLONE_IMAGE="$GFS_IMAGE" \
LABEL="${LABEL:-headless}" \
  pnpm --filter benchmark-explorer-server run bench
