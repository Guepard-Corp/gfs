#!/usr/bin/env bash
# B7 (#122): a rename looks like a drop plus an add, so it is a conflict.
. "$(dirname "$0")/../lib/common.sh"
case_begin B7 "source RENAME COLUMN is detected and treated as a conflict"
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null
src "ALTER TABLE orders RENAME COLUMN total TO amount;"
nudge
assert_match "$(q 'SELECT * FROM orders;')" "gfs: the source schema" "rename detected"
assert_match "$("$BIN" pull 2>&1)" "conflict" "reported as a conflict, not silently applied"
case_end
