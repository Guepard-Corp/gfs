#!/usr/bin/env bash
# H2 (#132): a frozen clone is DETACHED. Every table -- including one never read
# before the freeze -- answers with the source container STOPPED, and
# fetch/pull/status say "detached" instead of erroring. The source stays down
# for every assertion: that is the entire promise.
. "$(dirname "$0")/../lib/common.sh"
case_begin H2 "a frozen clone answers everything with the source stopped"
fixture_simple; clone_now
W0=$(src_write_counter)
val "SELECT count(*) FROM orders;" >/dev/null       # one table copied lazily
freeze_now || { echo "    freeze log:"; freeze_log | tail -6 | sed 's/^/      /'; }
assert_true "$(P "SELECT frozen::text FROM gfs.clone_mode;")" "the clone is marked frozen"
assert_true "$(clone_state notes whole_cached)" "a never-read table was copied by the freeze"
assert_source_untouched "$W0"

docker stop "$SRC_CT" >/dev/null 2>&1               # and it STAYS stopped below

assert_query_eq "SELECT count(*) FROM orders;" 3 "orders answers offline"
assert_query_eq "SELECT count(*) FROM notes;" 1 "notes (never read before the freeze) answers offline"
assert_query_eq "SELECT count(*) FROM other;" 1 "other answers offline"

FR=$("$BIN" fetch 2>&1); assert_match "$FR" "detached" "fetch says the clone is detached, without probing"
FC=$("$BIN" fetch --check 2>&1); assert_match "$FC" "detached" "fetch --check does not probe a sealed source"
PL=$("$BIN" pull 2>&1); assert_match "$PL" "detached" "pull says there is no source to sync"
ST=$("$BIN" status 2>&1); assert_match "$ST" "frozen" "status shows the frozen state"
case_end
