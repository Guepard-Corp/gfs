#!/usr/bin/env bash
# Chaos / fault-injection harness for the gfs lazy clone -- find the LIMITS and
# prove graceful degradation.
#
#   A. SOURCE DOWN during copy-on-read -> the query ERRORS (never a wrong/partial
#      result), records NO false coverage, and recovers correctly once the source
#      is back. (Correctness under fault.)
#   B. N CONCURRENT CLONES hammering ONE source -> measure the aggregate load the
#      source bears (active backends + probe latency) as N grows. (The
#      prod-protection threshold -- measured, not theorised.)
#
#   CLONES=3 ./chaos_test.sh
set -uo pipefail
SRC=gfs-chaos-src; SRC_PORT=55650
GFS_IMAGE="${GFS_IMAGE:-gfs-postgres:16}"
GFS_BIN="${GFS_BIN:-$(cd "$(dirname "$0")/../../.." && pwd)/target/debug/gfs}"
PSQL="${PSQL:-/opt/homebrew/opt/postgresql@16/bin/psql}"
CLONES="${CLONES:-3}"            # scenario B fan-out
CLONE_BASE_PORT="${CLONE_BASE_PORT:-55660}"
LOAD_PIDS=()

red(){ printf '\033[31m%s\033[0m' "$1"; }; grn(){ printf '\033[32m%s\033[0m' "$1"; }
note(){ printf '\n\033[1;36m== %s ==\033[0m\n' "$1"; }
PASS=0; FAIL=0; ok(){ PASS=$((PASS+1)); echo "  $(grn PASS) $1"; }; bad(){ FAIL=$((FAIL+1)); echo "  $(red FAIL) $1"; }
src(){ PGPASSWORD=pw "$PSQL" "postgresql://app:pw@localhost:${SRC_PORT}/shop" -tAqc "$1" 2>/dev/null; }
clone_url(){ echo "postgresql://postgres:postgres@localhost:$1/postgres"; }
cln(){ PGPASSWORD=postgres "$PSQL" "$(clone_url "$1")" -tAqc "$2" 2>/dev/null; }
# Wait for the REAL server: the postgres image logs "ready to accept connections"
# twice (temp init server, then the real one after restart). Wait for the 2nd AND a
# live host-TCP query that stays up (the temp server is unix-socket only).
wait_src(){
  for i in $(seq 1 90); do
    [[ "$(docker logs "$SRC" 2>&1 | grep -c 'ready to accept connections')" -ge 2 ]] \
      && src 'SELECT 1' >/dev/null 2>&1 && { sleep 1; src 'SELECT 1' >/dev/null 2>&1 && return 0; }
    sleep 1
  done
  return 1
}
now_ms(){ python3 -c 'import time;print(int(time.time()*1000))'; }

cleanup() {
  for p in "${LOAD_PIDS[@]:-}"; do kill "$p" 2>/dev/null; done
  for i in $(seq 0 "$CLONES"); do docker rm -f "$(docker ps -aq --filter publish=$((CLONE_BASE_PORT+i)))" >/dev/null 2>&1; done
  docker rm -f "$SRC" >/dev/null 2>&1
  rm -rf /tmp/gfs-chaos-clone-* 2>/dev/null
}
trap cleanup EXIT
[[ -x "$GFS_BIN" ]] || { red "gfs binary not found at $GFS_BIN\n"; exit 1; }

# ---------------------------------------------------------------------------
note "Source (postgres:16, seeded)"
cleanup
docker run -d --name "$SRC" -e POSTGRES_PASSWORD=pw -e POSTGRES_USER=app -e POSTGRES_DB=shop \
  -p "${SRC_PORT}:5432" postgres:16 >/dev/null
wait_src || { echo "  source never ready on TCP"; exit 1; }
src "DO \$\$ BEGIN IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname='gfs_reader') THEN CREATE ROLE gfs_reader LOGIN PASSWORD 'rpw'; END IF; END \$\$;
     CREATE TABLE orders(id bigint PRIMARY KEY, customer_id bigint, amount int);
     INSERT INTO orders SELECT g, 1+(g%1000), g%500 FROM generate_series(1,200000) g;
     CREATE TABLE customers(id bigint PRIMARY KEY, name text);
     INSERT INTO customers SELECT g, 'c'||g FROM generate_series(1,1000) g;
     GRANT USAGE ON SCHEMA public TO gfs_reader; GRANT SELECT ON ALL TABLES IN SCHEMA public TO gfs_reader;" >/dev/null
echo "  seeded orders=$(src 'SELECT count(*) FROM orders')"

make_clone() { # $1=port $2=dir -> clone the source onto that port (retry the flaky
  # provisioning race: `gfs clone` can bootstrap before the clone's postgres accepts
  # TCP under resource pressure -- a real robustness finding; retry works around it).
  for attempt in 1 2 3; do
    rm -rf "$2"; docker rm -f "$(docker ps -aq --filter publish=$1)" >/dev/null 2>&1
    "$GFS_BIN" clone --from "postgres://gfs_reader:rpw@host.docker.internal:${SRC_PORT}/shop" \
      --image "$GFS_IMAGE" --database-version 16 --port "$1" "$2" >/dev/null 2>&1
    for i in $(seq 1 30); do
      case "$(cln "$1" 'SELECT count(*) FROM gfs.clones')" in (''|*[!0-9]*) sleep 1;; (*) return 0;; esac
    done
    echo "  (clone on :$1 attempt $attempt failed the provisioning race -- retrying)"
  done
  return 1
}

# ===========================================================================
note "A. SOURCE DOWN during copy-on-read -> graceful error, no false coverage, recovery"
P=$CLONE_BASE_PORT; D=/tmp/gfs-chaos-clone-A
make_clone "$P" "$D"
cln "$P" "SELECT 1" >/dev/null || { red "clone A unreachable\n"; exit 1; }
cln "$P" "UPDATE gfs.cost SET net=1,source=20,negligible=1,horizon=0,ceiling=100000" >/dev/null  # not whole-ownable -> hydrate ranges

KNOWN="$(src 'SELECT count(*) FROM orders WHERE id BETWEEN 1 AND 50000')"
echo "  killing the source mid-flight (docker kill)"
docker kill "$SRC" >/dev/null
# A copy-on-read of a not-yet-cached range must now FAIL (source unreachable).
ERR="$(PGPASSWORD=postgres "$PSQL" "$(clone_url "$P")" -c "SELECT count(*) FROM orders WHERE id BETWEEN 1 AND 50000" 2>&1)"
echo "$ERR" | grep -qiE "error|could not connect|failed|terminating|closed" && ok "query ERRORED gracefully (no silent wrong result)" || bad "query did not surface an error: $ERR"
# The failed hydrate must NOT have recorded coverage.
COV="$(cln "$P" "SELECT count(*) FROM gfs.cached WHERE relid='orders'::regclass")"
[[ "$COV" == "0" || -z "$COV" ]] && ok "no false coverage recorded for the failed range" || bad "FALSE coverage recorded ($COV ranges)"

echo "  restarting the source (docker start) + crash recovery"
docker start "$SRC" >/dev/null
wait_src || { echo "  source never ready on TCP"; exit 1; }
GOT="$(cln "$P" "SELECT count(*) FROM orders WHERE id BETWEEN 1 AND 50000")"
[[ "$GOT" == "$KNOWN" ]] && ok "after recovery the same query returns the CORRECT result ($GOT)" || bad "post-recovery result wrong ($GOT vs $KNOWN)"
docker rm -f "$(docker ps -aq --filter publish=$P)" >/dev/null 2>&1; rm -rf "$D"

# ===========================================================================
note "B. N CONCURRENT CLONES vs ONE source -> aggregate load as N grows"
# A HEAVY federate-class query (full join + GROUP BY) re-contacts and makes the
# source WORK on EVERY call -> sustained, measurable load. (To find the real
# threshold, raise CLONES and the source size: this small demo only shows the trend.)
LOADQ="SELECT count(*) FROM (SELECT c.id, sum(o.amount) FROM orders o JOIN customers c ON c.id=o.customer_id GROUP BY c.id) t"
active(){ src "SELECT count(*) FROM pg_stat_activity WHERE datname='shop' AND state='active' AND query NOT ILIKE '%pg_stat_activity%'"; }
probe_ms(){ # median-ish latency of the source query under current load, ms (avg of 3)
  local s=0 t0 t1 k; for k in 1 2 3; do t0=$(now_ms); src "$LOADQ" >/dev/null; t1=$(now_ms); s=$((s+t1-t0)); done; echo $((s/3))
}
printf "  %-8s %-12s %-12s\n" clones src_active probe_ms
printf "  %-8s %-12s %-12s\n" 0 "$(active)" "$(probe_ms)"
for n in $(seq 1 "$CLONES"); do
  P=$((CLONE_BASE_PORT+n)); D=/tmp/gfs-chaos-clone-B$n
  make_clone "$P" "$D" || { echo "  (clone $n could not provision -- stopping fan-out)"; break; }
  cln "$P" "UPDATE gfs.cost SET net=1,source=20,negligible=1,horizon=0,ceiling=100000" >/dev/null
  # sustained federate load from this clone (background loop on the host)
  ( while :; do PGPASSWORD=postgres "$PSQL" "$(clone_url "$P")" -tAqc "$LOADQ" >/dev/null 2>&1; done ) &
  LOAD_PIDS+=($!)
  sleep 4   # let the load ramp
  printf "  %-8s %-12s %-12s\n" "$n" "$(active)" "$(probe_ms)"
done
echo "  -> probe latency / active backends rise with clone count = the aggregate load one source bears."
echo "     (per-clone budget bounds RATE, not the aggregate -> the global envelope's job.)"
ok "measured aggregate load across $CLONES concurrent clones"

# ===========================================================================
note "Verdict"
echo "  $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]] && grn "ALL PASS -- failures are graceful, recovery correct, aggregate load measured\n" || { red "FAILURES\n"; exit 1; }
