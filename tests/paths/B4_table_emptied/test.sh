#!/usr/bin/env bash
# B4 (#117): the source empties the table entirely.
. "$(dirname "$0")/../lib/common.sh"
case_begin B4 "source TRUNCATE leaves the clone reporting zero rows"
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null
src "TRUNCATE orders;"
nudge
assert_query_eq "SELECT count(*) FROM orders;" 0 "returns 0"
case_end
