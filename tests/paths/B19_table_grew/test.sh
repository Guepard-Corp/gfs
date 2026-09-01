#!/usr/bin/env bash
# B19 (#115): the table grows after clone time. The size used by the cost model
# was measured once, at clone time, so a table that was small then could be
# whole-copied long after it stopped being small.
#
# The guard runs at ONE point: inside the whole-own gate, just before committing
# to a copy. So the table has to be back ON THE LAZY PATH for it to be evaluated.
# Straight after the source grows, the table is `drifted` and reads federate,
# which never reaches the gate. A pull puts it back on the lazy path, and the
# next read is what exercises the guard.
. "$(dirname "$0")/../lib/common.sh"
case_begin B19 "a table that grew is re-measured before the clone commits to copying it"
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null
BEFORE=$(clone_state orders source_rows)

src "INSERT INTO orders SELECT g,'c'||g,g FROM generate_series(1000,20000) g;"
nudge
assert_query_eq "SELECT count(*) FROM orders;" 19004 "the answer is correct while federating"

"$BIN" pull >/dev/null 2>&1          # back onto the lazy path
val "SELECT count(*) FROM orders;" >/dev/null   # this read evaluates the whole-own gate
sleep 3
AFTER=$(clone_state orders source_rows)
[ "${AFTER:-0}" -gt "${BEFORE:-0}" ] 2>/dev/null \
  && ok "the recorded size was re-measured ($BEFORE -> $AFTER), so the cost decision uses the real number" \
  || no "source_rows still reads $AFTER (was $BEFORE): the cost model would decide on a stale size"
assert_query_eq "SELECT count(*) FROM orders;" 19004 "and the answer is still correct afterwards"
case_end
