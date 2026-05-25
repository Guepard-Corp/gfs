#!/usr/bin/env bash
# GFS k3s full matrix — logs to $LOG
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
GFS="${GFS_BIN:-$ROOT/target/debug/gfs}"
E2E="${GFS_E2E_DIR:-/tmp/gfs-k3s-e2e}"
LOG="${GFS_E2E_LOG:-/tmp/gfs-k3s-e2e.log}"

export KUBECONFIG="${KUBECONFIG:-$ROOT/.kube-remote/k3s.yaml}"
export GFS_RUNTIME_PROVIDER=kubernetes
export GFS_ALLOW_UNFROZEN_SNAPSHOT=1
export GFS_K8S_STORAGE_CLASS=openebs-zfs-gfs
export GFS_K8S_SNAPSHOT_CLASS=openebs-zfs-gfs-snapclass
export GFS_K8S_PVC_SIZE_GI=1
export GUEPARD_EXTERNAL_HOST="${GUEPARD_EXTERNAL_HOST:-${GFS_K8S_EXTERNAL_HOST:-52.32.232.60}}"
export GFS_K8S_EXTERNAL_HOST="${GFS_K8S_EXTERNAL_HOST:-$GUEPARD_EXTERNAL_HOST}"

exec > >(tee -a "$LOG") 2>&1

step() { echo ""; echo "========== $* =========="; }
run() { echo "+ $*"; "$@"; ec=$?; echo "exit=$ec"; return $ec; }

psql_cs() {
  local CS
  CS=$("$GFS" status --path "$E2E" --json | jq -r '.compute.connection_string')
  echo "connection_string=$CS"
  psql "$CS" "$@"
}

psql_rows() {
  psql_cs -c "select step, branch from gfs_e2e order by step;" 2>&1 || true
}

wait_pg() {
  for i in $(seq 1 60); do
    if psql_cs -c "select 1" &>/dev/null; then return 0; fi
    sleep 3
  done
  return 1
}

rm -rf "$E2E"
mkdir -p "$E2E"
cd "$E2E"

step "init"
: "${GFS_DB_USER:?set GFS_DB_USER}"
: "${GFS_DB_PASSWORD:?set GFS_DB_PASSWORD}"
: "${GFS_DB_NAME:=postgres}"
run "$GFS" init --database-provider postgres --database-version 17 \
  --database-user "$GFS_DB_USER" --database-password "$GFS_DB_PASSWORD" --database-name "$GFS_DB_NAME"

step "wait postgres"
wait_pg
run psql_cs -c "select version();"

step "status + log"
run "$GFS" status
run "$GFS" log

step "seed main"
psql_cs -c "create table if not exists gfs_e2e (x int, branch text, step text primary key);"
psql_cs -c "insert into gfs_e2e values (1,'main','init_main') on conflict do nothing;"
psql_rows

step "commit c1"
run "$GFS" commit -m "main c1"
psql_cs -c "insert into gfs_e2e values (2,'main','after_c1') on conflict do nothing;"
psql_rows

step "commit c2"
run "$GFS" commit -m "main c2"
C1=$("$GFS" log --full-hash --max-count 2 | grep -oE '[0-9a-f]{64}' | sed -n '2p')
echo "C1=$C1"

step "checkout c1"
run "$GFS" checkout "$C1"
wait_pg
psql_rows

step "checkout main"
run "$GFS" checkout main
wait_pg
psql_rows

step "branch feature"
run "$GFS" branch feature
run "$GFS" checkout feature
wait_pg
psql_rows

step "feature commit"
psql_cs -c "insert into gfs_e2e values (10,'feature','feature_c1') on conflict do nothing;"
run "$GFS" commit -m "feature c1"
psql_rows

step "main c3"
run "$GFS" checkout main
wait_pg
psql_cs -c "insert into gfs_e2e values (3,'main','main_c3') on conflict do nothing;"
run "$GFS" commit -m "main c3"
psql_rows

step "feature tip"
run "$GFS" checkout feature
wait_pg
psql_rows

step "checkout -b hotfix"
run "$GFS" checkout -b hotfix
wait_pg
psql_rows

step "branch hotfix2 -c"
run "$GFS" branch hotfix2 -c
wait_pg
psql_rows

step "branch list"
run "$GFS" branch

step "compute stop"
run "$GFS" compute stop
if psql_cs -c "select 1" 2>/dev/null; then echo "FAIL: still connected after stop"; exit 1; else echo "OK: connect refused after stop"; fi

step "compute start"
run "$GFS" compute start
wait_pg
psql_rows

step "compute restart"
psql_cs -c "insert into gfs_e2e values (99,'main','before_restart') on conflict do nothing;" || true
run "$GFS" compute restart
wait_pg
psql_cs -c "select step from gfs_e2e where step='before_restart';"
psql_rows

step "query"
run "$GFS" query "select count(*) from gfs_e2e"

step "branch -d hotfix"
run "$GFS" branch -d hotfix

step "kubectl snapshot"
kubectl -n gfs get pods,svc,pvc,volumesnapshot,statefulset -o wide
run "$GFS" log --full-hash

echo ""
echo "E2E matrix complete — log: $LOG"
