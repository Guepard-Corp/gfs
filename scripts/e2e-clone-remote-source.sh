#!/usr/bin/env bash
# Specialized tests against a REAL remote Postgres (not a throwaway container).
#
# Why this exists: the Rust e2e suite and the local shell suites all start a
# throwaway Postgres on host.docker.internal. That is fast and hermetic, but it
# cannot exercise the things that only go wrong at real scale and over a real
# link: the cost model's copy-vs-ask decision, calibration of network speed, and
# federation of a table far too large to copy.
#
# TWO MODES, because they have very different blast radius:
#
#   readonly  (default, SAFE on a production-shaped database)
#             Clones the real schemas and asserts on ROUTER DECISIONS and read
#             correctness. Writes nothing to the source. Use this against data
#             you care about.
#
#   drift     (MUTATES THE SOURCE, only ever inside its own scratch schema)
#             Creates gfs_test_<pid> on the source, seeds small fixtures, clones
#             with ?schema=<that schema> so nothing else is even mirrored, runs
#             the drift/repair scenarios, then drops the schema. Never touches a
#             pre-existing table.
#
# Usage:
#   scripts/e2e-clone-remote-source.sh readonly "postgresql://user:pw@host:5432/db?sslmode=require"
#   scripts/e2e-clone-remote-source.sh drift    "postgresql://user:pw@host:5432/db?sslmode=require"
#
# Or set GFS_TEST_SOURCE_URL and omit the second argument.
set -uo pipefail

MODE="${1:-readonly}"
URL="${2:-${GFS_TEST_SOURCE_URL:-}}"
BIN="${GFS_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release/gfs}"
IMAGE="${GFS_IMAGE:-gfs-postgres:16}"
WORK="${GFS_TEST_WORKDIR:-${TMPDIR:-/tmp}}/gfs-remote-e2e-$$"

pass=0; fail=0; skip=0
ok(){   echo "    PASS: $1"; pass=$((pass+1)); }
no(){   echo "    FAIL: $1"; fail=$((fail+1)); }
sk(){   echo "    SKIP: $1"; skip=$((skip+1)); }
die(){  echo "ABORT: $1" >&2; exit 90; }

# psql renders a boolean as t/f, but `expr::text` renders it as true/false, and
# comparing the wrong one produces a FALSE FAILURE that looks like a product bug.
# Normalise both, and refuse to guess at anything else.
tf(){ case "$(printf '%s' "$1" | tr -d '[:space:]')" in
        t|true|TRUE|True)   echo t ;;
        f|false|FALSE|False) echo f ;;
        *) echo "unparsed:$1" ;;
      esac }

[ -n "$URL" ] || die "no source URL (pass one, or set GFS_TEST_SOURCE_URL)"
[ -x "$BIN" ] || die "gfs binary not found at $BIN (cargo build --release)"
docker image inspect "$IMAGE" >/dev/null 2>&1 || die "image $IMAGE not present"

# psql against the SOURCE, through a throwaway client container so no local
# postgres client install is required and the TLS settings come from the URL.
S(){ docker run --rm postgres:16 psql "$URL" -qtAc "$1" 2>&1; }

SRC_HOST=$(printf '%s' "$URL" | sed -E 's|.*@([^:/?]+).*|\1|')
echo "source: $SRC_HOST    mode: $MODE"
S "SELECT 1" >/dev/null 2>&1 || die "cannot reach the source"

CLONE_URL="$URL"
SCRATCH=""
cleanup(){
  # GFS_KEEP=1 leaves the clone container and repo up for inspection. The scratch
  # schema is dropped regardless: never leave test tables behind on a real source.
  if [ -n "${GFS_KEEP:-}" ]; then
    echo "  GFS_KEEP set: leaving container ${CID:-none} and repo $WORK in place"
    [ -n "$SCRATCH" ] && S "DROP SCHEMA IF EXISTS $SCRATCH CASCADE;" >/dev/null 2>&1
    return
  fi
  [ -n "${CID:-}" ] && docker rm -f "$CID" >/dev/null 2>&1
  if [ -n "$SCRATCH" ]; then
    echo "  dropping scratch schema $SCRATCH on the source"
    S "DROP SCHEMA IF EXISTS $SCRATCH CASCADE;" >/dev/null 2>&1
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

#############################################################################
# drift mode: build an isolated scratch schema so nothing real is touched
#############################################################################
if [ "$MODE" = "drift" ]; then
  SCRATCH="gfs_test_$$"
  echo "  creating scratch schema $SCRATCH (the ONLY thing this mode writes)"
  S "CREATE SCHEMA $SCRATCH;
     CREATE TABLE $SCRATCH.orders(id int PRIMARY KEY, customer text, total int);
     INSERT INTO $SCRATCH.orders VALUES (1,'Alice',50),(2,'Bob',30),(3,'Carol',20);
     CREATE TABLE $SCRATCH.notes(id int PRIMARY KEY, body text);
     INSERT INTO $SCRATCH.notes VALUES (1,'n1');
     ANALYZE $SCRATCH.orders; ANALYZE $SCRATCH.notes;" >/dev/null 2>&1 \
    || die "could not create the scratch schema (needs CREATE on the database)"
  # ?schema= scopes the clone, so the real tables are not even mirrored.
  # Plain shell expansion, not sed: BSD sed rejects `t` followed by `;`, which
  # silently produced an EMPTY url here and would have cloned the wrong thing.
  case "$URL" in
    *\?*) CLONE_URL="${URL%%\?*}?schema=$SCRATCH&${URL#*\?}" ;;
    *)    CLONE_URL="$URL?schema=$SCRATCH" ;;
  esac
  [ -n "$CLONE_URL" ] || die "internal: scoped clone url came out empty"
  case "$CLONE_URL" in *"schema=$SCRATCH"*) : ;; *) die "internal: clone url is not scoped to $SCRATCH" ;; esac
  echo "  clone scoped to: ?schema=$SCRATCH"
fi

#############################################################################
# clone
#############################################################################
mkdir -p "$WORK" || die "cannot create $WORK"
echo "  cloning (this pays a real schema dump over the network) ..."
START=$(date +%s)
"$BIN" clone --from "$CLONE_URL" --image "$IMAGE" "$WORK" > "$WORK/clone.log" 2>&1
CLONE_RC=$?
ELAPSED=$(( $(date +%s) - START ))
cd "$WORK" || die "cannot enter $WORK"
CID=$(grep container_name .gfs/config.toml 2>/dev/null | sed 's/.*= *"//;s/".*//')
if [ $CLONE_RC -ne 0 ] || [ -z "$CID" ]; then
  echo "ABORT: clone did not complete (rc=$CLONE_RC) after ${ELAPSED}s"
  tail -12 "$WORK/clone.log"; exit 91
fi
for i in $(seq 1 60); do docker exec "$CID" psql -U postgres -qtAc "SELECT 1" >/dev/null 2>&1 && break; sleep 1; done
docker exec "$CID" psql -U postgres -qtAc "SELECT 1" >/dev/null 2>&1 || die "clone container never became queryable"
echo "  cloned in ${ELAPSED}s"
P(){ docker exec -i "$CID" psql -U postgres -tAc "$1" 2>&1; }
val(){ "$BIN" query "$1" 2>&1 | tail -3 | head -1 | tr -d ' '; }

#############################################################################
# assertions common to both modes
#############################################################################
echo "=============== SHELL: the clone is complete and holds no rows ==============="
SRC_T=$(S "SELECT count(*) FROM pg_stat_user_tables${SCRATCH:+ WHERE schemaname='$SCRATCH'}")
REG=$(P "SELECT count(*) FROM gfs.clone_source;")
[ "$REG" = "$SRC_T" ] && ok "every source table registered copy-on-read ($REG of $SRC_T)" \
                      || no "registered $REG but the source has $SRC_T"
UNREG=$(P "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
           WHERE c.relkind='r' AND n.nspname NOT LIKE 'pg_%' AND n.nspname NOT IN ('information_schema','gfs')
             AND n.nspname NOT LIKE 'gfs_remote_%'
             AND NOT EXISTS (SELECT 1 FROM gfs.clone_source cs WHERE cs.relid=c.oid);")
[ "$UNREG" = "0" ] && ok "no unregistered local table (an unregistered one reads as silently empty)" \
                   || no "$UNREG local table(s) not registered"

echo "=============== BASELINE: drift detection is anchored ==============="
BL=$(P "SELECT count(*) FROM gfs.source_table_baseline;")
[ "$BL" = "$REG" ] && ok "every registered table has a drift baseline ($BL)" || no "only $BL baselines for $REG tables"
LSN=$(P "SELECT count(*) FROM gfs.source_baseline;")
[ "$LSN" = "1" ] && ok "source LSN anchored" || no "expected 1 baseline row, got $LSN"
DRIFT=$(P "SELECT count(*) FROM gfs.source_drift();" | grep -v WARNING | tail -1)
[ "$DRIFT" = "0" ] && ok "a fresh clone reports no drift" || no "fresh clone already reports $DRIFT finding(s)"

echo "=============== COST: calibration produced usable weights ==============="
read -r NET SRCW NEG < <(P "SELECT net || ' ' || source || ' ' || negligible FROM gfs.cost;" | tr -d '\r')
echo "    net=$NET  source=$SRCW  negligible=$NEG"
awk -v n="$NET" 'BEGIN{exit !(n+0>0)}' && ok "net is above zero (a zero link would stampede whole copies, #112)" \
                                       || no "net=$NET: the link measured as free, #112 has regressed"
awk -v s="$SRCW" 'BEGIN{exit !(s+0>0)}' && ok "source cost is above zero" || no "source=$SRCW"

#############################################################################
# readonly mode: the real value, decisions at real scale
#############################################################################
if [ "$MODE" = "readonly" ]; then
  echo "=============== COST at REAL SCALE: the biggest table must NOT be copied ==============="
  BIGN=$(P "SELECT m.src_table FROM gfs.source_map m JOIN gfs.clone_source cs ON cs.relid=m.relid
            ORDER BY cs.source_rows DESC LIMIT 1;")
  BIGR=$(P "SELECT max(source_rows) FROM gfs.clone_source;")
  echo "    largest registered table: $BIGN ($BIGR rows)"
  if [ -z "$BIGR" ] || [ "$BIGR" -lt 1000000 ] 2>/dev/null; then
    sk "no table large enough to exercise the cost gate (largest is $BIGR rows)"
  else
    # The REAL predicate from route.rs, not just the ceiling. A table can sit far
    # under the ceiling and still be correctly refused because it fails the
    # amortization half. Asserting only on the ceiling reports a false failure.
    #   E(own)  = net * row_bytes * Tr
    #   E(fed)  = source * max(Tr,1)
    #   ownable = E(own) <= negligible OR (E(own) <= ceiling AND E(own) <= (H+1)*E(fed))
    read -r EOWN EFED NEGV CEIL OWNABLE < <(P "
      SELECT round((c.net*cs.row_bytes*cs.source_rows)::numeric,1) || ' ' ||
             round((c.source*greatest(cs.source_rows,1))::numeric,1) || ' ' ||
             round(c.negligible::numeric,3) || ' ' ||
             round(c.ceiling::numeric,1) || ' ' ||
             ( (c.net*cs.row_bytes*cs.source_rows) <= c.negligible
               OR ( (c.net*cs.row_bytes*cs.source_rows) <= c.ceiling
                AND (c.net*cs.row_bytes*cs.source_rows)
                    <= (least(cs.access_count,c.horizon)+1)*(c.source*greatest(cs.source_rows,1)) ) )::text
        FROM gfs.cost c, gfs.clone_source cs ORDER BY cs.source_rows DESC LIMIT 1;" | tr -d '\r')
    echo "    E(own)=$EOWN  E(fed)=$EFED  negligible=$NEGV  ceiling=$CEIL"
    [ "$(tf "$OWNABLE")" = "f" ] && ok "$BIGN is NOT whole-ownable, so a full copy can never be attempted" \
                         || no "$BIGN evaluates as whole-ownable ('$OWNABLE'): the clone would try to copy it"
    CACHED=$(P "SELECT whole_cached::text FROM gfs.clone_source ORDER BY source_rows DESC LIMIT 1;" | tr -d '\r')
    [ "$(tf "$CACHED")" = "f" ] && ok "$BIGN is not marked whole-cached on a fresh clone" \
                        || no "$BIGN reports whole_cached='$CACHED' on a FRESH clone (nothing has been read yet)"
  fi

  echo "=============== READS are correct against the live source ==============="
  T1=$(P "SELECT m.src_schema || '.' || m.src_table FROM gfs.source_map m
          JOIN gfs.clone_source cs ON cs.relid=m.relid ORDER BY cs.source_rows ASC LIMIT 1;")
  if [ -n "$T1" ]; then
    C_SRC=$(S "SELECT count(*) FROM $T1")
    C_CLN=$(val "SELECT count(*) FROM $T1;")
    [ "$C_SRC" = "$C_CLN" ] && ok "$T1 count matches the source ($C_CLN)" \
                            || no "$T1: clone says '$C_CLN', source says '$C_SRC'"
  else
    sk "no table available to read"
  fi

  echo "=============== GOLDEN RULE: the source was never written to ==============="
  # Any write would move the source's write counters. Compare a probe to itself.
  W1=$(S "SELECT COALESCE(sum(n_tup_ins+n_tup_upd+n_tup_del),0) FROM pg_stat_user_tables")
  val "SELECT count(*) FROM $T1;" >/dev/null 2>&1
  W2=$(S "SELECT COALESCE(sum(n_tup_ins+n_tup_upd+n_tup_del),0) FROM pg_stat_user_tables")
  [ "$W1" = "$W2" ] && ok "source write counters unchanged by clone activity ($W2)" \
                    || no "source counters moved $W1 -> $W2: something WROTE to the source"
fi

#############################################################################
# drift mode: the full drift/repair matrix, in the scratch schema only
#############################################################################
if [ "$MODE" = "drift" ]; then
  SS="$SCRATCH"
  src(){ S "$1" >/dev/null 2>&1; }
  docker exec -i "$CID" psql -U postgres -qc "UPDATE gfs.sync_policy SET check_interval='1 second';" >/dev/null 2>&1
  nudge(){ for i in 1 2 3; do "$BIN" query "SELECT count(*) FROM $SS.notes;" >/dev/null 2>&1; sleep 3; done; }

  "$BIN" query "SELECT count(*) FROM $SS.orders;" >/dev/null 2>&1   # cache it

  echo "=============== D1: source INSERT is picked up ==============="
  src "INSERT INTO $SS.orders VALUES (4,'Dave',40);"; nudge
  V=$(val "SELECT count(*) FROM $SS.orders;")
  [ "$V" = "4" ] && ok "returns 4 (not the stale 3)" || no "returned '$V', expected 4"

  echo "=============== D2: source TRUNCATE is picked up (#117) ==============="
  src "TRUNCATE $SS.orders;"; nudge
  V=$(val "SELECT count(*) FROM $SS.orders;")
  [ "$V" = "0" ] && ok "returns 0: TRUNCATE was detected" || no "returned '$V', expected 0"

  echo "=============== D3: schema change is reported clearly (#122) ==============="
  src "ALTER TABLE $SS.notes ADD COLUMN tag text;"; nudge
  OUT=$("$BIN" query "SELECT * FROM $SS.notes;" 2>&1)
  echo "$OUT" | grep -q "gfs: the source schema" && ok "clear gfs error, not a raw remote one" \
    || no "expected a gfs message, got: $(echo "$OUT" | head -1)"
  OUT=$("$BIN" pull 2>&1)
  echo "$OUT" | grep -q "repaired" && ok "pull repaired the added column" || no "pull did not repair: $(echo "$OUT" | tr '\n' ' ' | head -c 120)"

  echo "=============== D4: local writes are never destroyed ==============="
  "$BIN" query "INSERT INTO $SS.notes VALUES (99,'mine',NULL);" >/dev/null 2>&1
  src "INSERT INTO $SS.notes VALUES (2,'n2',NULL);"; nudge
  OUT=$("$BIN" pull 2>&1)
  echo "$OUT" | grep -qi "conflict" && ok "reported as a conflict" || no "no conflict reported"
  V=$(val "SELECT count(*) FROM $SS.notes WHERE id=99;")
  [ "$V" = "1" ] && ok "the local row survived the pull" || no "local row lost (count=$V)"
fi

echo
echo "=========== RESULTS ($MODE): $pass passed, $fail failed, $skip skipped ==========="
exit $fail
