#!/usr/bin/env bash
# Shared harness for the per-path tests in tests/paths/.
#
# One script per path in the divergence document, so a failure names the exact
# case (B4, D3, ...) rather than "something in the drift suite broke".
#
# Everything here exists because of a false result seen in practice. Each guard
# is load-bearing:
#   * `pg_isready` reports OK while the server is still reaching a consistent
#     state, and the setup DDL that follows is then silently lost. We demand a
#     real `SELECT 1`.
#   * A clone can exit non-zero OR leave a STUB config (version + description,
#     no container). Both are setup failures and must ABORT, never be scored as
#     product failures: one bad clone otherwise prints a dozen convincing FAILs.
#   * psql prints a boolean as t/f but `expr::text` prints true/false. Comparing
#     the wrong rendering invents failures. See tf().
#   * `timeout` does not exist on macOS. Never use it.
#   * Ports are derived per case, so two paths never collide.
#
# A path script looks like:
#     . "$(dirname "$0")/lib/common.sh"
#     case_begin B1 "source INSERT is visible" --expect pass
#     fixture_simple; clone_now
#     ...
#     case_end

set -uo pipefail

# ---------------------------------------------------------------- environment
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
BIN="${GFS_BIN:-$ROOT/target/release/gfs}"
IMAGE="${GFS_IMAGE:-gfs-postgres:16}"
PGIMAGE="${GFS_SOURCE_IMAGE:-postgres:16}"
WORKROOT="${GFS_TEST_WORKDIR:-${TMPDIR:-/tmp}}/gfs-paths"

CASE_ID=""; CASE_DESC=""; CASE_EXPECT="pass"
pass=0; fail=0; skipped=0
SRC_CT=""; CID=""; WORK=""; PORT=""

ok(){ echo "    PASS  $1"; pass=$((pass+1)); }
no(){ echo "    FAIL  $1"; fail=$((fail+1)); }
sk(){ echo "    SKIP  $1"; skipped=$((skipped+1)); }
# ABORT = the environment failed, NOT the product. Distinct exit code so the
# master runner can report it separately instead of counting it as a defect.
abort(){ echo "    ABORT $1" >&2; exit 90; }

# psql renders booleans two different ways depending on the cast; normalise both
# and refuse to guess at anything else.
tf(){ case "$(printf '%s' "${1:-}" | tr -d '[:space:]')" in
        t|true|TRUE|True)    echo t ;;
        f|false|FALSE|False) echo f ;;
        *) echo "unparsed:${1:-}" ;;
      esac }

# ------------------------------------------------------------------ lifecycle
case_begin(){
  CASE_ID="$1"; CASE_DESC="$2"; shift 2
  while [ $# -gt 0 ]; do
    case "$1" in
      --expect) CASE_EXPECT="$2"; shift 2 ;;   # pass | open
      *) shift ;;
    esac
  done
  # deterministic per-case port so parallel or repeated runs never collide
  local h; h=$(printf '%s' "$CASE_ID" | cksum | awk '{print $1}')
  PORT=$(( 56000 + (h % 3000) ))
  SRC_CT="gfs-path-src-$(printf '%s' "$CASE_ID" | tr 'A-Z' 'a-z')"
  WORK="$WORKROOT/$CASE_ID-$$"
  echo "=== $CASE_ID  $CASE_DESC"
  [ "$CASE_EXPECT" = "open" ] && echo "    (declared KNOWN-OPEN: documents today's behaviour)"
  [ -x "$BIN" ] || abort "gfs binary not found at $BIN (cargo build --release)"
  docker image inspect "$IMAGE" >/dev/null 2>&1 || abort "image $IMAGE not present"
  trap _cleanup EXIT
}

_cleanup(){
  [ -n "${GFS_KEEP:-}" ] && { echo "    (GFS_KEEP: leaving $SRC_CT / $CID / $WORK)"; return; }
  [ -n "$CID" ]    && docker rm -f "$CID"    >/dev/null 2>&1
  [ -n "$SRC_CT" ] && docker rm -f "$SRC_CT" >/dev/null 2>&1
  [ -n "$WORK" ]   && rm -rf "$WORK"
  return 0
}

case_end(){
  echo "    ---- $CASE_ID: $pass passed, $fail failed, $skipped skipped"
  if [ "$CASE_EXPECT" = "open" ]; then
    # A known-open path is not a defect. But if it starts passing, say so loudly:
    # that means the document is now out of date.
    if [ "$fail" -eq 0 ] && [ "$pass" -gt 0 ]; then
      echo "    NOTE  $CASE_ID was declared KNOWN-OPEN but every assertion passed."
      echo "          If this is a real fix, update the document and flip --expect pass."
      exit 3
    fi
    exit 0
  fi
  exit "$fail"
}

# --------------------------------------------------------------------- source
_start_source(){
  docker rm -f "$SRC_CT" >/dev/null 2>&1
  local i
  for i in $(seq 1 15); do
    docker run -d --name "$SRC_CT" -p "$PORT:5432" -e POSTGRES_PASSWORD=x "$PGIMAGE" >/dev/null 2>&1 && break
    docker rm -f "$SRC_CT" >/dev/null 2>&1; sleep 2
  done
  docker ps --format '{{.Names}}' | grep -qx "$SRC_CT" || abort "source container did not start"
  # a real query, not pg_isready: the server accepts connections before it is ready
  for i in $(seq 1 60); do docker exec "$SRC_CT" psql -U postgres -qtAc "SELECT 1" >/dev/null 2>&1 && break; sleep 1; done
  docker exec "$SRC_CT" psql -U postgres -qtAc "SELECT 1" >/dev/null 2>&1 \
    || abort "source never became queryable"
}

# Run DDL/DML on the SOURCE. Fails loudly: silently-lost setup is how a test
# ends up asserting against a fixture that was never created.
src(){ docker exec -i "$SRC_CT" psql -U postgres -q -v ON_ERROR_STOP=1 -c "$1" >/dev/null 2>&1 \
       || { echo "    (source statement failed: $(printf '%s' "$1" | head -c 60))" >&2; return 1; } }
srcq(){ docker exec -i "$SRC_CT" psql -U postgres -qtAc "$1" 2>&1; }

# ------------------------------------------------------------------- fixtures
# Every fixture ends with an unrelated `other` table. Reads of it are how a
# background drift check gets a chance to run and COMMIT without touching the
# table under test.
fixture_simple(){
  _start_source
  src "CREATE TABLE orders(id int PRIMARY KEY, customer text, total int);
       INSERT INTO orders VALUES (1,'Alice',50),(2,'Bob',30),(3,'Carol',20);
       CREATE TABLE notes(id int PRIMARY KEY, body text);
       INSERT INTO notes VALUES (1,'n1');
       CREATE TABLE other(id int PRIMARY KEY, v text);
       INSERT INTO other VALUES (1,'a');
       ANALYZE;" || abort "fixture_simple DDL failed"
}

fixture_bulk(){   # a table big enough that a whole copy is visible in the stats
  _start_source
  src "CREATE TABLE orders(id int PRIMARY KEY, customer text, total int);
       INSERT INTO orders SELECT g,'c'||g,g FROM generate_series(1,5000) g;
       CREATE TABLE other(id int PRIMARY KEY, v text);
       INSERT INTO other VALUES (1,'a');
       ANALYZE;" || abort "fixture_bulk DDL failed"
}

fixture_sql(){    # arbitrary extra DDL on top of the simple fixture
  _start_source
  src "CREATE TABLE other(id int PRIMARY KEY, v text); INSERT INTO other VALUES (1,'a');
       $1
       ANALYZE;" || abort "fixture_sql DDL failed"
}

# ---------------------------------------------------------------------- clone
clone_now(){   # $1 optional extra query string, e.g. "schema=public"
  mkdir -p "$WORK" || abort "cannot create $WORK"
  local url="postgres://postgres:x@host.docker.internal:$PORT/postgres"
  [ $# -gt 0 ] && url="$url?$1"
  "$BIN" clone --from "$url" --image "$IMAGE" "$WORK" > "$WORK/clone.log" 2>&1
  local rc=$?
  cd "$WORK" || abort "cannot enter $WORK"
  CID=$(grep container_name .gfs/config.toml 2>/dev/null | sed 's/.*= *"//;s/".*//')
  if [ $rc -ne 0 ] || [ -z "$CID" ]; then
    echo "    clone log:"; tail -8 "$WORK/clone.log" | sed 's/^/      /'
    abort "clone did not complete (rc=$rc, container='$CID')"
  fi
  local i
  for i in $(seq 1 45); do docker exec "$CID" psql -U postgres -qtAc "SELECT 1" >/dev/null 2>&1 && break; sleep 1; done
  docker exec "$CID" psql -U postgres -qtAc "SELECT 1" >/dev/null 2>&1 \
    || abort "clone container never became queryable"
  # make background drift verdicts go stale fast so nudge() actually triggers one
  docker exec -i "$CID" psql -U postgres -qc "UPDATE gfs.sync_policy SET check_interval='1 second';" >/dev/null 2>&1
}

clone_must_fail(){   # for paths where refusing to clone IS the correct behaviour
  mkdir -p "$WORK" || abort "cannot create $WORK"
  "$BIN" clone --from "postgres://postgres:x@host.docker.internal:$PORT/postgres" \
        --image "$IMAGE" "$WORK" > "$WORK/clone.log" 2>&1
  local rc=$?
  CID=$(grep container_name "$WORK/.gfs/config.toml" 2>/dev/null | sed 's/.*= *"//;s/".*//')
  [ $rc -ne 0 ] && return 0
  return 1
}
clone_log(){ cat "$WORK/clone.log" 2>/dev/null; }

# ----------------------------------------------------------------- clone side
# CAREFUL: P() runs psql INSIDE the clone, where the planner hook is active.
# There is no bypass GUC, so `P "SELECT count(*) FROM orders"` is NOT a look at
# the local copy: it goes through the router and will happily hydrate the table,
# changing the very state you were trying to observe. Selecting from a gfs
# catalog table (gfs.clone_source, gfs.drift_state, pg_catalog) is safe, because
# the hook only intercepts REGISTERED clone relations.
#
# To ask "what does the clone hold locally, right now", use local_bytes() or
# clone_state() below. Both read metadata and never scan the table.
P(){ docker exec -i "$CID" psql -U postgres -tAc "$1" 2>&1; }          # raw SQL on the clone
q(){ "$BIN" query "$1" 2>&1; }                                          # through the CLI
val(){ "$BIN" query "$1" 2>&1 | tail -3 | head -1 | tr -d ' '; }        # single scalar

# Physical size of the local heap. Never scans, so the hook cannot fire: this is
# the honest way to prove "nothing has been copied yet" (0 bytes = no pages).
local_bytes(){ P "SELECT pg_relation_size('$1'::regclass);"; }
# A column of gfs.clone_source for one table, by its name ON THE SOURCE.
clone_state(){ P "SELECT cs.$2::text FROM gfs.clone_source cs JOIN gfs.source_map m ON m.relid=cs.relid WHERE m.src_table='$1';"; }

# A read of an UNRELATED table lets the enqueued background drift check run in a
# transaction that commits. Reading the table under test would instead block on
# the very condition being measured.
nudge(){ local i; for i in 1 2 3; do "$BIN" query "SELECT count(*) FROM other;" >/dev/null 2>&1; sleep 3; done; }

# Wait until a table is actually held whole locally. A whole-own is completed by
# the BACKGROUND worker, so `pull; query; sleep 3` is a race: the copy may not
# have landed yet, and a test that then stops the source reports a self-healing
# failure that is really a timing bug. Poll the state instead of guessing.
# Returns 0 once cached, 1 on timeout.
wait_until_cached(){   # $1 = source table name, $2 = seconds (default 45)
  local t="$1" limit="${2:-45}" i
  for i in $(seq 1 "$limit"); do
    [ "$(tf "$(clone_state "$t" whole_cached)")" = "t" ] && return 0
    "$BIN" query "SELECT count(*) FROM $t;" >/dev/null 2>&1   # give the router a reason to act
    sleep 1
  done
  return 1
}

# Run a body with the source stopped, then always restart it. Without the
# restart-on-failure an early exit leaves the container down and every later
# path in the sweep aborts.
with_source_down(){    # $1 = sql to evaluate while the source is unreachable
  docker stop "$SRC_CT" >/dev/null 2>&1
  local out; out=$(val "$1")
  docker start "$SRC_CT" >/dev/null 2>&1
  local i; for i in $(seq 1 30); do docker exec "$SRC_CT" psql -U postgres -qtAc "SELECT 1" >/dev/null 2>&1 && break; sleep 1; done
  printf '%s' "$out"
}

# ------------------------------------------------------------------ assertions
assert_eq(){        # actual expected message
  [ "$1" = "$2" ] && ok "$3 ($1)" || no "$3: got '$1', expected '$2'"; }
assert_query_eq(){  # sql expected message
  local got; got=$(val "$1"); assert_eq "$got" "$2" "$3"; }
assert_src_eq(){    # sql expected message  (asserts on the SOURCE)
  local got; got=$(srcq "$1"); assert_eq "$got" "$2" "$3"; }
assert_match(){     # text regex message
  printf '%s' "$1" | grep -qiE "$2" && ok "$3" || no "$3: got '$(printf '%s' "$1" | head -c 90)'"; }
assert_nomatch(){
  printf '%s' "$1" | grep -qiE "$2" && no "$3 (matched '$2')" || ok "$3"; }
assert_false(){     # boolean-ish value message
  [ "$(tf "$1")" = "f" ] && ok "$2" || no "$2: got '$1'"; }
assert_true(){
  [ "$(tf "$1")" = "t" ] && ok "$2" || no "$2: got '$1'"; }

# The golden rule, assertable from any path: GFS must never write to the source.
src_write_counter(){ srcq "SELECT COALESCE(sum(n_tup_ins+n_tup_upd+n_tup_del),0) FROM pg_stat_user_tables"; }
assert_source_untouched(){  # $1 = counter captured earlier
  local now; now=$(src_write_counter)
  assert_eq "$now" "$1" "the source was never written to"; }
