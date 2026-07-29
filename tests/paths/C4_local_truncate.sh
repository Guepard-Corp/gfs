#!/usr/bin/env bash
# C4: you empty the table on the clone. Federating would repopulate it from the
# source, undoing exactly what you asked for.
. "$(dirname "$0")/lib/common.sh"
case_begin C4 "a locally emptied table is not repopulated from the source"
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null
q "DELETE FROM orders;" >/dev/null 2>&1
assert_query_eq "SELECT count(*) FROM orders;" 0 "the table is empty locally"
nudge
assert_query_eq "SELECT count(*) FROM orders;" 0 "it stays empty (federating would refill it)"
assert_src_eq "SELECT count(*) FROM orders" "3" "the source still has its rows"
case_end
