#!/usr/bin/env bash
# A2: a partly copied table. The clone holds some rows locally and must still
# answer correctly for the rows it does not hold.
. "$(dirname "$0")/../lib/common.sh"
case_begin A2 "a partly copied table still answers correctly for everything"
fixture_bulk; clone_now
val "SELECT count(*) FROM orders WHERE id BETWEEN 1 AND 50;" >/dev/null
sleep 2
assert_query_eq "SELECT count(*) FROM orders;" 5000 "the full count is right regardless of what is held locally"
assert_query_eq "SELECT count(*) FROM orders WHERE id BETWEEN 4900 AND 5000;" 101 "a slice never touched locally is still correct"
case_end
