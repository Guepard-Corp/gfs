#!/usr/bin/env bash
# B3 (#118): a deleted row must stop being returned. This is the case where
# serving a stale local copy returns rows that exist nowhere upstream.
. "$(dirname "$0")/../lib/common.sh"
case_begin B3 "source DELETE removes the row from the clone's answers"
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null
src "DELETE FROM orders WHERE id=3;"
nudge
assert_query_eq "SELECT count(*) FROM orders WHERE id=3;" 0 "the deleted row is gone"
assert_query_eq "SELECT count(*) FROM orders;" 2 "the total reflects the delete"
case_end
