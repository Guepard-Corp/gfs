#!/usr/bin/env bash
# E1 (#131): a clone is NOT a point-in-time snapshot.
#
# The property under test is reproducibility, not correctness. Reads ARE kept
# current by drift detection, which is precisely the problem: the same query
# returns different answers over time with no local action at all, so a clone
# cannot be used as a stable branch for a repeatable test run.
#
# Declared KNOWN-OPEN. #132 (snapshot mode) is the fix.
. "$(dirname "$0")/../lib/common.sh"
case_begin E1 "a clone changes under you: the same query gives different answers over time" --expect open
fixture_simple; clone_now
FIRST=$(val "SELECT count(*) FROM orders;")
src "INSERT INTO orders VALUES (4,'Dave',40);"
nudge
SECOND=$(val "SELECT count(*) FROM orders;")
echo "    (same query, no local action: first read '$FIRST', later read '$SECOND')"
[ "$FIRST" = "$SECOND" ] \
  && ok "the clone held still: repeated reads agree, so it behaves as a snapshot" \
  || no "the clone shifted under the reader ($FIRST then $SECOND) with no local action (#131, fixed by #132)"
case_end
