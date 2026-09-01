#!/usr/bin/env bash
# H6 (#132): freezing a frozen clone is a no-op that says so (nothing is
# re-copied), pull/fetch report "detached", and `gfs export` of a frozen clone
# completes with the source STOPPED (materialize must not try to warm).
. "$(dirname "$0")/../lib/common.sh"
case_begin H6 "freeze is idempotent; pull/fetch/export behave on a frozen clone"
fixture_simple; clone_now
W0=$(src_write_counter)
freeze_now || { echo "    freeze log:"; freeze_log | tail -6 | sed 's/^/      /'; }
R1=$(P "SELECT COALESCE(sum(rows_fetched),0) FROM gfs.clone_stats;")

freeze_now
RC=$?
[ "$RC" -eq 0 ] && ok "second freeze exits 0" || no "second freeze failed (rc=$RC)"
assert_match "$(freeze_log)" "already frozen" "and says it is already frozen"
R2=$(P "SELECT COALESCE(sum(rows_fetched),0) FROM gfs.clone_stats;")
assert_eq "$R2" "$R1" "no table was re-copied by the second freeze"
assert_source_untouched "$W0"

PL=$("$BIN" pull 2>&1);  assert_match "$PL" "detached" "pull reports detached"
PA=$("$BIN" pull --auto on 2>&1); assert_match "$PA" "no effect" "auto-pull switch explains it has no effect"
FR=$("$BIN" fetch 2>&1); assert_match "$FR" "detached" "fetch reports detached"

docker stop "$SRC_CT" >/dev/null 2>&1
EX=$("$BIN" export --format sql 2>&1); RC=$?
[ "$RC" -eq 0 ] && ok "export of a frozen clone completes with the source stopped" \
                || { no "export failed offline (rc=$RC)"; echo "$EX" | tail -4 | sed 's/^/      /'; }
docker start "$SRC_CT" >/dev/null 2>&1
case_end
