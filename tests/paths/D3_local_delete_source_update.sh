#!/usr/bin/env bash
# D3: you deleted a row, the source updated it. Applying the source would
# resurrect a row you deliberately removed.
. "$(dirname "$0")/lib/common.sh"
case_begin D3 "you deleted, the source updated: the row stays deleted"
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null
q "DELETE FROM orders WHERE id=1;" >/dev/null 2>&1
src "UPDATE orders SET total=777 WHERE id=1;"
nudge
assert_query_eq "SELECT count(*) FROM orders WHERE id=1;" 0 "the row is not resurrected"
assert_match "$("$BIN" pull 2>&1)" "conflict" "pull reports it rather than silently reviving it"
case_end
