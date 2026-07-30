#!/usr/bin/env bash
# B5 (#124): an added column is additive, so pull may apply it.
. "$(dirname "$0")/../lib/common.sh"
case_begin B5 "source ADD COLUMN is detected and repaired by pull"
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null
src "ALTER TABLE orders ADD COLUMN discount int DEFAULT 7;"
nudge
assert_match "$(q 'SELECT * FROM orders;')" "gfs: the source schema" "a clear gfs error, not a raw remote one"
assert_match "$("$BIN" pull 2>&1)" "repaired" "pull repairs it"
assert_match "$(q 'SELECT * FROM orders;')" "discount" "the new column is visible afterwards"
case_end
