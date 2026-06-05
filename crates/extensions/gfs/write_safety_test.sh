#!/usr/bin/env bash
# Proves the write-safety guard: a WRITE (UPDATE/DELETE) whose scan would FEDERATE
# is whole-hydrated LOCALLY instead of being pushed to the source -- so the SOURCE
# stays byte-for-byte untouched while the clone diverges. Reuses the persistent
# TPC-H SF1 source volume from tpch_validate.sh.
set -uo pipefail
SRC_PORT=55610; CLONE_PORT=55613
SRC_NAME=gfs-tpch-src-sf1; SRC_VOL=gfs-tpch-vol-sf1
CLONE_DIR="$(mktemp -d)/clone"
GFS_BIN="${GFS_BIN:-$(cd "$(dirname "$0")/../../.." && pwd)/target/debug/gfs}"
PSQL="${PSQL:-/opt/homebrew/opt/postgresql@16/bin/psql}"
GFS_IMAGE=gfs-postgres:16
red(){ printf '\033[31m%s\033[0m' "$1"; }; grn(){ printf '\033[32m%s\033[0m' "$1"; }
note(){ printf '\n\033[1;36m== %s ==\033[0m\n' "$1"; }
src(){ PGPASSWORD=pw "$PSQL" "postgresql://app:pw@localhost:${SRC_PORT}/tpch" -tAqc "$1" 2>/dev/null; }
cln(){ PGPASSWORD=postgres "$PSQL" "postgresql://postgres:postgres@localhost:${CLONE_PORT}/postgres" -tAqc "$1" 2>/dev/null; }
PASS=0; FAIL=0
ok(){ PASS=$((PASS+1)); echo "  $(grn PASS) $1"; }; bad(){ FAIL=$((FAIL+1)); echo "  $(red FAIL) $1"; }
cleanup(){ local c; c="$(docker ps -aq --filter publish=${CLONE_PORT})"; [[ -n "$c" ]] && docker rm -f "$c" >/dev/null 2>&1; rm -rf "$(dirname "$CLONE_DIR")"; }
trap cleanup EXIT

note "Source (reuse $SRC_VOL)"
for c in $(docker ps --filter "publish=${SRC_PORT}" --format '{{.Names}}' | grep '^gfs-tpch-src-' | grep -vx "$SRC_NAME"); do docker stop "$c" >/dev/null 2>&1; done
docker rm -f "$SRC_NAME" >/dev/null 2>&1
docker run -d --name "$SRC_NAME" --shm-size=1g -e POSTGRES_PASSWORD=pw -e POSTGRES_USER=app -e POSTGRES_DB=tpch \
  -v "${SRC_VOL}:/var/lib/postgresql/data" -p "${SRC_PORT}:5432" postgres:16 -c work_mem=256MB >/dev/null
for i in $(seq 1 60); do docker exec "$SRC_NAME" pg_isready -U app -d tpch >/dev/null 2>&1 && break; sleep 1; done
[[ "$(src 'SELECT count(*) FROM orders')" -gt 0 ]] || { red "source has no data — run tpch_validate.sh SF=1 first\n"; exit 1; }
echo "  orders=$(src 'SELECT count(*) FROM orders')"

note "Clone"
"$GFS_BIN" clone --from "postgresql://app:pw@host.docker.internal:${SRC_PORT}/tpch" --image "$GFS_IMAGE" --port "$CLONE_PORT" "$CLONE_DIR" >/dev/null 2>&1
cln "SELECT 1" >/dev/null || { red "clone unreachable\n"; exit 1; }
# Low ceiling so orders is NOT whole-ownable -> a no-/non-key-predicate scan federate-classifies.
cln "UPDATE gfs.cost SET net=1, source=20, negligible=100000, horizon=0, ceiling=10000000" >/dev/null

note "UPDATE that federate-classifies (subquery qual on the not-ownable orders)"
SRC_BEFORE="$(src "SELECT o_comment FROM orders WHERE o_orderkey=1")"
echo "  source o_comment@1 before: '${SRC_BEFORE}'"
cln "UPDATE orders SET o_comment='GFS_GUARD_WRITE' WHERE o_orderkey = (SELECT min(o_orderkey) FROM orders)" >/dev/null 2>&1
SRC_AFTER="$(src "SELECT o_comment FROM orders WHERE o_orderkey=1")"
CLN_AFTER="$(cln "SELECT o_comment FROM orders WHERE o_orderkey=1")"
KIND="$(cln "SELECT whole_cached||'/'||federate_calls FROM gfs.clones WHERE clone='orders'")"
echo "  source o_comment@1 after : '${SRC_AFTER}'   (orders whole_cached/feder=$KIND)"
echo "  clone  o_comment@1 after : '${CLN_AFTER}'"
[[ "$SRC_AFTER" == "$SRC_BEFORE" ]] && ok "SOURCE untouched by the UPDATE" || bad "SOURCE was written! ('$SRC_AFTER')"
[[ "$CLN_AFTER" == "GFS_GUARD_WRITE" ]] && ok "clone diverged locally (write applied)" || bad "local write not applied ('$CLN_AFTER')"

note "DELETE that federate-classifies"
SRC_CNT_BEFORE="$(src "SELECT count(*) FROM orders")"
cln "DELETE FROM orders WHERE o_orderkey < (SELECT 200 FROM orders LIMIT 1)" >/dev/null 2>&1
SRC_CNT_AFTER="$(src "SELECT count(*) FROM orders")"
CLN_CNT="$(cln "SELECT count(*) FROM orders")"
echo "  source count before/after: $SRC_CNT_BEFORE / $SRC_CNT_AFTER ;  clone count: $CLN_CNT"
[[ "$SRC_CNT_AFTER" == "$SRC_CNT_BEFORE" ]] && ok "SOURCE row count unchanged by the DELETE" || bad "SOURCE rows deleted!"
[[ "$CLN_CNT" -lt "$SRC_CNT_BEFORE" ]] && ok "clone deleted rows locally" || bad "local delete not applied"

note "Verdict"
echo "  $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]] && grn "ALL PASS — writes stay local, source untouched\n" || { red "FAILURES\n"; exit 1; }
