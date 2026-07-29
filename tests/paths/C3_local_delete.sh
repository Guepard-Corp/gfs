#!/usr/bin/env bash
# C3: you delete on the clone. Asking the source would RESURRECT the row, so a
# diverged table must stop federating.
. "$(dirname "$0")/lib/common.sh"
case_begin C3 "a locally deleted row is not resurrected by the source"
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null
q "DELETE FROM orders WHERE id=2;" >/dev/null 2>&1
assert_query_eq "SELECT count(*) FROM orders WHERE id=2;" 0 "the row is gone locally"
nudge
assert_query_eq "SELECT count(*) FROM orders WHERE id=2;" 0 "it stays gone (federating would bring it back)"
case_end
