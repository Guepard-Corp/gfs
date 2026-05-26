#!/usr/bin/env bash
# End-to-end demo:
#   1. start the source PostgreSQL 14 (docker-compose) with all extensions
#   2. apply schema + seed, run the app against the source (baseline)
#   3. `gfs clone` the source into a local GFS repo
#   4. run the SAME app against the clone and compare
#
# Requires: Docker Desktop, Node 20+, and a built `gfs` binary.
# macOS/Windows: the clone reaches the source via host.docker.internal.
set -euo pipefail

cd "$(dirname "$0")/.."          # examples/clone-drizzle-demo
DEMO_DIR="$(pwd)"
REPO_ROOT="$(cd ../.. && pwd)"

GFS_BIN="${GFS_BIN:-$REPO_ROOT/target/debug/gfs}"
REMOTE_HOST="${REMOTE_HOST:-host.docker.internal}"   # how the clone reaches the source
SOURCE_PORT="${SOURCE_PORT:-55432}"
CLONE_PORT="${CLONE_PORT:-55433}"
CLONE_DIR="$DEMO_DIR/clone-repo"

SOURCE_URL="postgres://app:app@localhost:${SOURCE_PORT}/appdb"
CLONE_REMOTE_URL="postgres://app:app@${REMOTE_HOST}:${SOURCE_PORT}/appdb"
CLONE_APP_URL="postgres://postgres:postgres@localhost:${CLONE_PORT}/postgres"

step() { printf '\n\033[1;34m== %s ==\033[0m\n' "$1"; }

if [[ ! -x "$GFS_BIN" ]]; then
  echo "gfs binary not found at $GFS_BIN"
  echo "Build it first:  (cd $REPO_ROOT && cargo build -p gfs-cli)"
  echo "or set GFS_BIN=/path/to/gfs"
  exit 1
fi

step "Install Node dependencies"
[[ -d node_modules ]] || npm install

step "Start source PostgreSQL 14"
docker compose up -d
echo -n "  waiting for source to be healthy"
until [[ "$(docker inspect -f '{{.State.Health.Status}}' gfs-demo-source 2>/dev/null)" == "healthy" ]]; do
  echo -n "."; sleep 1
done
echo " ok"

step "Apply schema + seed (source)"
DATABASE_URL="$SOURCE_URL" npm run --silent db:setup
DATABASE_URL="$SOURCE_URL" npm run --silent db:seed

step "Run app against SOURCE (baseline)"
DATABASE_URL="$SOURCE_URL" npm run --silent app

step "gfs clone -> $CLONE_DIR"
# Free the clone port from any container left by a previous run.
OLD_CLONE="$(docker ps -q --filter "publish=${CLONE_PORT}")"
[[ -n "$OLD_CLONE" ]] && docker rm -f "$OLD_CLONE" >/dev/null
rm -rf "$CLONE_DIR"
mkdir -p "$CLONE_DIR"
"$GFS_BIN" clone --from "$CLONE_REMOTE_URL" --database-version 14 --port "$CLONE_PORT" "$CLONE_DIR"

step "Run app against CLONE"
DATABASE_URL="$CLONE_APP_URL" npm run --silent app

step "Done"
cat <<EOF
Source : $SOURCE_URL
Clone  : $CLONE_APP_URL   (GFS repo: $CLONE_DIR)

The source must stay running while you query the clone (copy-on-read).
Tear down with:
  docker compose down -v
  (cd "$CLONE_DIR" && "$GFS_BIN" compute stop) ; rm -rf "$CLONE_DIR"
EOF
