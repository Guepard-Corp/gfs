#!/usr/bin/env bash
# Agent-style E2E: exercises every gfs remote command path against multipass stack.
set -euo pipefail

ROOT="$(cd -- "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
GFS_ROOT="${GFS_ROOT:-$ROOT/gfs}"
GFS="${GFS_BIN:-$GFS_ROOT/target/debug/gfs}"
RENDERED="${GFS_E2E_RENDERED:-$ROOT/data-platform-v3/infra/multipass/.rendered/dev.env}"
E2E_DIR="${GFS_REMOTE_E2E_DIR:-/tmp/gfs-remote-agent-e2e}"
LOG="${GFS_REMOTE_E2E_LOG:-/tmp/gfs-remote-agent-e2e.log}"

[[ -f "$RENDERED" ]] || { echo "missing $RENDERED (start multipass stack first)" >&2; exit 1; }
# shellcheck source=/dev/null
source "$RENDERED"

: "${GUEPARD_CP_URL:?}"
: "${DP_IP:?}"
: "${GUEPARD_ENGINE_ORG:?}"

CONSOLE_URL="${GUEPARD_CONSOLE_URL:-http://127.0.0.1:${SERVER_PORT:-4096}}"
ORG="${GUEPARD_ENGINE_ORG}"
NID="${GUEPARD_ENGINE_NODE_ID:-}"

exec > >(tee -a "$LOG") 2>&1

step() { echo ""; echo "========== $* =========="; }
fail() { echo "FAIL: $*" >&2; exit 1; }

query_count() {
  local out
  out=$("$GFS" --json query --path "$E2E_DIR" "SELECT count(*) FROM $1" | jq -r '.output // empty')
  echo "$out" | grep -Eo '[0-9]+' | head -1
}

wait_compute_status() {
  local want="$1" tries="${2:-40}" delay="${3:-3}"
  local status=""
  for _ in $(seq 1 "$tries"); do
    status=$("$GFS" --json status --path "$E2E_DIR" 2>/dev/null | jq -r '.compute.compute_status // empty' || true)
    [[ "$status" == "$want" ]] && return 0
    sleep "$delay"
  done
  fail "timed out waiting for compute_status=$want (last=$status)"
}

step "build gfs"
unset OPENSSL_LIB_DIR OPENSSL_INCLUDE_DIR OPENSSL_STATIC 2>/dev/null || true
(cd "$GFS_ROOT" && cargo build -p gfs-cli -q)

if [[ -z "$NID" ]]; then
  NID="$(curl -fsS "${GUEPARD_CP_URL}/orgs/${ORG}/nodes" | jq -r '.[0].id // empty')"
fi
[[ -n "$NID" ]] || fail "no engine node id"

step "login"
export GUEPARD_CONSOLE_URL="$CONSOLE_URL"
export GUEPARD_SUPABASE_URL="${GUEPARD_SUPABASE_URL:-${SUPABASE_URL:-http://127.0.0.1:18785}}"
export GUEPARD_SUPABASE_ANON_KEY="${GUEPARD_SUPABASE_ANON_KEY:-${SUPABASE_ANON_KEY:-}}"
export GUEPARD_LOGIN_EMAIL="${GUEPARD_LOGIN_EMAIL:-owner@guepard.run}"
export GUEPARD_LOGIN_PASSWORD="${GUEPARD_LOGIN_PASSWORD:-testingpassword}"
[[ -n "$GUEPARD_SUPABASE_ANON_KEY" ]] || fail "set GUEPARD_SUPABASE_ANON_KEY or SUPABASE_ANON_KEY"

"$GFS" login --email "$GUEPARD_LOGIN_EMAIL" --password "$GUEPARD_LOGIN_PASSWORD"

CREDS_FILE="${HOME}/.config/guepard/credentials.toml"
SAVED_TOKEN="$(grep -E '^access_token' "$CREDS_FILE" 2>/dev/null | head -1 | sed -E 's/^access_token = "([^"]+)".*/\1/')"
[[ -n "$SAVED_TOKEN" ]] || fail "no access_token in credentials after login"

step "token login (agent/CI path)"
"$GFS" login --token "$SAVED_TOKEN" >/dev/null

step "remote show"
"$GFS" --json remote show | jq -e '.access_token == "<set>" and .console_url' >/dev/null \
  || fail "remote show should mask token and expose console_url"

step "remote nodes"
"$GFS" --json remote nodes | jq -e 'type == "array" and length > 0' >/dev/null \
  || fail "remote nodes empty"

rm -rf "$E2E_DIR"
mkdir -p "$E2E_DIR"

step "init --remote --json"
INIT_ARGS=(--json init --remote "$E2E_DIR" \
  --database-provider postgres --database-version 17 \
  --remote-node "$NID")
[[ -n "${GUEPARD_PROJECT:-}" ]] && INIT_ARGS+=(--project "$GUEPARD_PROJECT")
INIT_JSON="$("$GFS" "${INIT_ARGS[@]}")"
echo "$INIT_JSON" | jq -e '.deployment_id and .database_id and .connection' >/dev/null \
  || fail "init json incomplete"
DEPLOY_ID="$(echo "$INIT_JSON" | jq -r '.deployment_id')"
CP_DB_ID="$(echo "$INIT_JSON" | jq -r '.database_id')"

step "status after init"
STATUS_JSON="$("$GFS" --json status --path "$E2E_DIR")"
echo "$STATUS_JSON" | jq -e '.compute and .remote' >/dev/null || fail "status shape"
INIT_STATUS="$(echo "$STATUS_JSON" | jq -r '.compute.compute_status // empty')"
[[ "$INIT_STATUS" == "running" ]] || fail "expected running after init, got $INIT_STATUS"

step "query DDL/DML"
"$GFS" --json query --path "$E2E_DIR" \
  "CREATE TABLE IF NOT EXISTS gfs_agent_e2e (id int primary key, note text)"
"$GFS" --json query --path "$E2E_DIR" "INSERT INTO gfs_agent_e2e VALUES (1, 'before')"

step "commit + log"
COMMIT1="$("$GFS" --json commit --path "$E2E_DIR" -m "agent c1")"
HASH1="$(echo "$COMMIT1" | jq -r '.hash')"
[[ -n "$HASH1" && "$HASH1" != "null" ]] || fail "commit1 hash missing"
"$GFS" --json query --path "$E2E_DIR" "INSERT INTO gfs_agent_e2e VALUES (2, 'after')"
COMMIT2="$("$GFS" --json commit --path "$E2E_DIR" -m "agent c2")"
HASH2="$(echo "$COMMIT2" | jq -r '.hash')"
[[ -n "$HASH2" && "$HASH2" != "null" ]] || fail "commit2 hash missing"

step "log (plain)"
LOG_JSON="$("$GFS" --json log --path "$E2E_DIR" -n 10)"
echo "$LOG_JSON" | jq -e 'if type == "array" then length > 0 else .commits end' >/dev/null \
  || fail "log empty or invalid"

step "checkout to first commit"
CO_JSON="$("$GFS" --json checkout --path "$E2E_DIR" "$HASH1")"
echo "$CO_JSON" | jq -e '.result.commit or .commit' >/dev/null || fail "checkout missing commit"
ROWS="$(query_count gfs_agent_e2e)"
[[ "$ROWS" == "1" ]] || fail "expected 1 row after checkout, got $ROWS"

step "branch list"
"$GFS" --json branch --path "$E2E_DIR" | jq -e '.branches' >/dev/null || fail "branch list"

step "branch create rejected on primary (linear history)"
set +e
BRANCH_ERR="$("$GFS" --json branch --path "$E2E_DIR" "agent-feat" "$HASH1" 2>&1)"
BRANCH_RC=$?
set -e
[[ "$BRANCH_RC" -ne 0 ]] || fail "branch create should fail on primary deployment"
echo "$BRANCH_ERR" | jq -e '.error.message | test("clone|linear|primary")' >/dev/null \
  || fail "branch create error not actionable: $BRANCH_ERR"

step "branch delete rejected"
set +e
DEL_ERR="$("$GFS" --json branch --path "$E2E_DIR" -d "agent-feat" 2>&1)"
DEL_RC=$?
set -e
[[ "$DEL_RC" -ne 0 ]] || fail "branch delete should fail on remote"
echo "$DEL_ERR" | jq -e '.error.message' >/dev/null || fail "branch delete error not json"

step "schema show rejected"
set +e
SHOW_ERR="$("$GFS" --json schema show "$HASH1" --path "$E2E_DIR" 2>&1)"
SHOW_RC=$?
set -e
[[ "$SHOW_RC" -ne 0 ]] || fail "schema show should fail on remote"
echo "$SHOW_ERR" | jq -e '.error.message | test("not supported|remote")' >/dev/null \
  || fail "schema show error not actionable"

step "compute logs rejected"
set +e
LOGS_ERR="$("$GFS" --json compute --path "$E2E_DIR" logs 2>&1)"
LOGS_RC=$?
set -e
[[ "$LOGS_RC" -ne 0 ]] || fail "compute logs should fail on remote"
echo "$LOGS_ERR" | jq -e '.error.message | test("not supported|remote|logs")' >/dev/null \
  || fail "compute logs error not actionable"

step "checkout to second commit"
"$GFS" --json checkout --path "$E2E_DIR" "$HASH2" >/dev/null
ROWS="$(query_count gfs_agent_e2e)"
[[ "$ROWS" == "2" ]] || fail "expected 2 rows after checkout to c2, got $ROWS"

step "log --graph"
"$GFS" --json log --path "$E2E_DIR" --graph \
  | jq -e 'type == "array" or .commits or .nodes' >/dev/null || fail "log graph"

step "schema extract"
"$GFS" --json schema extract --path "$E2E_DIR" | jq -e 'type == "object"' >/dev/null \
  || fail "schema extract"

step "schema diff between commits"
"$GFS" --json schema diff --path "$E2E_DIR" "$HASH1" "$HASH2" \
  | jq -e 'type == "object"' >/dev/null || fail "schema diff"

step "compute stop"
"$GFS" --json compute --path "$E2E_DIR" stop >/dev/null
wait_compute_status stopped 20 2

step "compute start"
"$GFS" --json compute --path "$E2E_DIR" start >/dev/null
wait_compute_status running 40 3

step "query after resume"
"$GFS" --json query --path "$E2E_DIR" "SELECT 1 AS ok" | jq -e '.output' >/dev/null \
  || fail "query after resume"

step "destroy"
"$GFS" --json compute --path "$E2E_DIR" stop || true
DESTROY_JSON="$("$GFS" --json remote destroy --path "$E2E_DIR")"
echo "$DESTROY_JSON" | jq -e '.destroyed == true' >/dev/null || fail "destroy"

echo ""
echo "OK agent remote e2e (deployment=$DEPLOY_ID cp_db=$CP_DB_ID log=$LOG)"
