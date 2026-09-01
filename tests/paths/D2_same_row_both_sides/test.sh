#!/usr/bin/env bash
# D2: you changed a row, the source changed the same row. There is no correct
# automatic answer, so the user's version must win and it must be reported.
. "$(dirname "$0")/../lib/common.sh"
case_begin D2 "both sides changed the same row: the local version wins, loudly"
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null
q "UPDATE orders SET total=111 WHERE id=1;" >/dev/null 2>&1
src "UPDATE orders SET total=222 WHERE id=1;"
nudge
assert_query_eq "SELECT total FROM orders WHERE id=1;" 111 "the local value survives"
assert_match "$("$BIN" pull 2>&1)" "conflict" "pull reports a conflict"
assert_query_eq "SELECT total FROM orders WHERE id=1;" 111 "still the local value after pull"
case_end
