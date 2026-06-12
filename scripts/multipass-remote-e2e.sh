#!/usr/bin/env bash
# E2E: gfs remote mode against multipass stack (console + Supabase + CP/DP).
set -euo pipefail

ROOT="$(cd -- "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
GFS_ROOT="${GFS_ROOT:-$ROOT/gfs}"
GFS="${GFS_BIN:-$GFS_ROOT/target/debug/gfs}"
RENDERED="${GFS_E2E_RENDERED:-$ROOT/data-platform-v3/infra/multipass/.rendered/dev.env}"
E2E_DIR="${GFS_REMOTE_E2E_DIR:-/tmp/gfs-remote-e2e}"
LOG="${GFS_REMOTE_E2E_LOG:-/tmp/gfs-remote-e2e.log}"

[[ -f "$RENDERED" ]] || { echo "missing $RENDERED (start multipass stack first)" >&2; exit 1; }
# shellcheck source=/dev/null
source "$RENDERED"

: "${GUEPARD_CP_URL:?}"
: "${DP_IP:?}"

CONSOLE_URL="${GUEPARD_CONSOLE_URL:-http://127.0.0.1:${SERVER_PORT:-4096}}"
AS_URL="${GUEPARD_AUTOSCALER_URL:-http://${DP_IP}:9090}"
ORG="${GUEPARD_ENGINE_ORG:?}"

exec > >(tee -a "$LOG") 2>&1

step() { echo ""; echo "========== $* =========="; }
fail() { echo "FAIL: $*" >&2; exit 1; }

query_count() {
  local out
  out=$("$GFS" --json query --path "$E2E_DIR" "SELECT count(*) FROM $1" | jq -r '.output // empty')
  echo "$out" | grep -Eo '[0-9]+' | head -1
}

step "build gfs"
unset OPENSSL_LIB_DIR OPENSSL_INCLUDE_DIR OPENSSL_STATIC 2>/dev/null || true
(cd "$GFS_ROOT" && cargo build -p gfs-cli -q)

step "resolve engine node"
NID="${GUEPARD_ENGINE_NODE_ID:-}"
if [[ -z "$NID" ]]; then
  NID="$(curl -fsS "${GUEPARD_CP_URL}/orgs/${ORG}/nodes" | jq -r '.[0].id // empty')"
fi
[[ -n "$NID" ]] || fail "no engine node id"
echo "node_id=$NID console=$CONSOLE_URL"

step "login"
export GUEPARD_CONSOLE_URL="$CONSOLE_URL"
export GUEPARD_SUPABASE_URL="${GUEPARD_SUPABASE_URL:-${SUPABASE_URL:-http://127.0.0.1:18785}}"
export GUEPARD_SUPABASE_ANON_KEY="${GUEPARD_SUPABASE_ANON_KEY:-${SUPABASE_ANON_KEY:-}}"
export GUEPARD_LOGIN_EMAIL="${GUEPARD_LOGIN_EMAIL:-owner@guepard.run}"
export GUEPARD_LOGIN_PASSWORD="${GUEPARD_LOGIN_PASSWORD:-testingpassword}"
[[ -n "$GUEPARD_SUPABASE_ANON_KEY" ]] || fail "set GUEPARD_SUPABASE_ANON_KEY or SUPABASE_ANON_KEY in dev.env"

"$GFS" login --email "$GUEPARD_LOGIN_EMAIL" --password "$GUEPARD_LOGIN_PASSWORD"

rm -rf "$E2E_DIR"
mkdir -p "$E2E_DIR"

step "init --remote --json"
INIT_JSON="$("$GFS" --json init --remote "$E2E_DIR" \
  --database-provider postgres --database-version 17 \
  --remote-node "$NID")"
echo "$INIT_JSON" | jq -e '.deployment_id and .database_id and .connection' >/dev/null \
  || fail "init json missing deployment_id/database_id/connection"

DEPLOY_ID="$(echo "$INIT_JSON" | jq -r '.deployment_id')"
CP_DB_ID="$(echo "$INIT_JSON" | jq -r '.database_id')"

step "VCS + query"
"$GFS" --json query --path "$E2E_DIR" "CREATE TABLE IF NOT EXISTS gfs_remote_e2e (id int primary key, note text)"
"$GFS" --json query --path "$E2E_DIR" "INSERT INTO gfs_remote_e2e VALUES (1, 'before')"
COMMIT1="$("$GFS" --json commit --path "$E2E_DIR" -m "e2e before")"
HASH1="$(echo "$COMMIT1" | jq -r '.hash')"
[[ -n "$HASH1" && "$HASH1" != "null" ]] || fail "commit1 hash missing"
"$GFS" --json query --path "$E2E_DIR" "INSERT INTO gfs_remote_e2e VALUES (2, 'after')"
"$GFS" --json commit --path "$E2E_DIR" -m "e2e after" >/dev/null
"$GFS" --json checkout --path "$E2E_DIR" "$HASH1"
ROWS="$(query_count gfs_remote_e2e)"
[[ "$ROWS" == "1" ]] || fail "expected 1 row after checkout to first commit, got $ROWS"

step "branch + log graph + schema"
"$GFS" --json branch --path "$E2E_DIR" | jq -e '.branches' >/dev/null
"$GFS" --json log --path "$E2E_DIR" --graph | jq -e 'type == "array" or .commits' >/dev/null
"$GFS" --json schema extract --path "$E2E_DIR" | jq -e 'type == "object"' >/dev/null

step "lifecycle pause/resume"
"$GFS" --json compute --path "$E2E_DIR" stop
# `gfs status` exits 1 when compute is stopped — ignore exit code when parsing JSON.
STATUS="$("$GFS" --json status --path "$E2E_DIR" 2>/dev/null | jq -r '.compute.compute_status // empty' || true)"
[[ "$STATUS" == "stopped" ]] || fail "expected stopped, got $STATUS"
"$GFS" --json compute --path "$E2E_DIR" start
for _ in $(seq 1 40); do
  STATUS="$("$GFS" --json status --path "$E2E_DIR" 2>/dev/null | jq -r '.compute.compute_status // empty' || true)"
  [[ "$STATUS" == "running" ]] && break
  sleep 3
done
[[ "$STATUS" == "running" ]] || fail "expected running after resume, got $STATUS"

step "autoscaler health"
curl -fsS --max-time 10 "$AS_URL/health" >/dev/null || echo "WARN: autoscaler health skipped ($AS_URL)"

step "destroy"
"$GFS" --json compute --path "$E2E_DIR" stop || true
"$GFS" --json remote destroy --path "$E2E_DIR"

echo ""
echo "OK remote e2e (deployment=$DEPLOY_ID cp_db=$CP_DB_ID log=$LOG)"
