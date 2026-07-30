#!/usr/bin/env bash
# D4: you updated a row, the source deleted it. Applying the source would
# discard an edit you made deliberately.
. "$(dirname "$0")/../lib/common.sh"
case_begin D4 "you updated, the source deleted: your row is kept and reported"
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null
q "UPDATE orders SET total=424 WHERE id=2;" >/dev/null 2>&1
src "DELETE FROM orders WHERE id=2;"
nudge
assert_query_eq "SELECT total FROM orders WHERE id=2;" 424 "your updated row survives"
assert_match "$("$BIN" pull 2>&1)" "conflict" "pull reports it rather than deleting your row"
assert_query_eq "SELECT total FROM orders WHERE id=2;" 424 "still there after the pull"
case_end
