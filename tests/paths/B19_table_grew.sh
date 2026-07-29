#!/usr/bin/env bash
# B19 (#115): the table grows after clone time. The size used by the cost model
# was measured once, at clone time, so a table that was small then could be
# whole-copied long after it stopped being small.
. "$(dirname "$0")/lib/common.sh"
case_begin B19 "a table that grew is re-measured before the clone commits to copying it"
fixture_simple; clone_now
BEFORE=$(clone_state orders source_rows)
src "INSERT INTO orders SELECT g,'c'||g,g FROM generate_series(1000,20000) g;"
nudge
val "SELECT count(*) FROM orders;" >/dev/null
sleep 2
AFTER=$(clone_state orders source_rows)
[ "${AFTER:-0}" -gt "${BEFORE:-0}" ] 2>/dev/null \
  && ok "the recorded source size was refreshed ($BEFORE -> $AFTER), so the cost decision uses the real number" \
  || no "source_rows still reads $AFTER (was $BEFORE): the cost model is deciding on a stale size"
assert_query_eq "SELECT count(*) FROM orders;" 19004 "and the answer is still correct"
case_end
