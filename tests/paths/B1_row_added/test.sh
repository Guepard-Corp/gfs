#!/usr/bin/env bash
# B1 (#118): the source adds a row after the clone cached the table.
. "$(dirname "$0")/../lib/common.sh"
case_begin B1 "source INSERT is visible to the clone"
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null      # cache it
src "INSERT INTO orders VALUES (4,'Dave',40);"
nudge
assert_query_eq "SELECT count(*) FROM orders;" 4 "returns the new count, not the cached 3"
case_end
