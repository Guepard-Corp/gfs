#!/usr/bin/env bash
# Agent-style gfs remote e2e against AWS dev (console NodePort on CP).
set -euo pipefail

ROOT="$(cd -- "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
GFS_ROOT="${GFS_ROOT:-$ROOT/gfs}"
GFS="${GFS_BIN:-$GFS_ROOT/target/debug/gfs}"
RENDERED="${GFS_E2E_RENDERED:-$ROOT/data-platform-v3/infra/aws/.rendered/aws.env}"
E2E_DIR="${GFS_REMOTE_E2E_DIR:-/tmp/gfs-remote-aws-e2e}"
LOG="${GFS_REMOTE_E2E_LOG:-/tmp/gfs-remote-aws-e2e.log}"

[[ -f "$RENDERED" ]] || {
  echo "missing $RENDERED — run: data-platform-v3/scripts/aws/gather-aws-env.sh" >&2
  exit 1
}
# shellcheck source=/dev/null
source "$RENDERED"

: "${GUEPARD_CONSOLE_URL:?}"
: "${GUEPARD_ENGINE_NODE_ID:?}"
: "${GUEPARD_SUPABASE_URL:?}"
: "${GUEPARD_SUPABASE_ANON_KEY:?}"

export GUEPARD_LOGIN_EMAIL="${GUEPARD_LOGIN_EMAIL:-owner@guepard.run}"
export GUEPARD_LOGIN_PASSWORD="${GUEPARD_LOGIN_PASSWORD:-testingpassword}"

exec env \
  GFS_E2E_RENDERED="$RENDERED" \
  GFS_REMOTE_E2E_DIR="$E2E_DIR" \
  GFS_REMOTE_E2E_LOG="$LOG" \
  GUEPARD_CONSOLE_URL="$GUEPARD_CONSOLE_URL" \
  GUEPARD_SUPABASE_URL="$GUEPARD_SUPABASE_URL" \
  GUEPARD_SUPABASE_ANON_KEY="$GUEPARD_SUPABASE_ANON_KEY" \
  GUEPARD_LOGIN_EMAIL="$GUEPARD_LOGIN_EMAIL" \
  GUEPARD_LOGIN_PASSWORD="$GUEPARD_LOGIN_PASSWORD" \
  bash "$GFS_ROOT/scripts/multipass-remote-agent-e2e.sh"
