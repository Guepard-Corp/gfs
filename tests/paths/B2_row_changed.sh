#!/usr/bin/env bash
# B2 (#118): the source updates a row the clone already holds.
. "$(dirname "$0")/lib/common.sh"
case_begin B2 "source UPDATE is visible to the clone"
fixture_simple; clone_now
val "SELECT total FROM orders WHERE id=2;" >/dev/null
src "UPDATE orders SET total=999 WHERE id=2;"
nudge
assert_query_eq "SELECT total FROM orders WHERE id=2;" 999 "returns the new value, not the cached 30"
case_end
