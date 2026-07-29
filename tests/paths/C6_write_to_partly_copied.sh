#!/usr/bin/env bash
# C6: you write to a table only partly copied. The write must land on complete
# data, so the table is filled in first rather than accepting a write against a
# fragment.
. "$(dirname "$0")/lib/common.sh"
case_begin C6 "writing to a partly copied table completes the copy first"
fixture_bulk; clone_now
val "SELECT count(*) FROM orders WHERE id BETWEEN 1 AND 20;" >/dev/null
sleep 2
q "INSERT INTO orders VALUES (999999,'Mine',1);" >/dev/null 2>&1
sleep 2
assert_query_eq "SELECT count(*) FROM orders;" 5001 "all 5000 source rows plus the local one"
assert_query_eq "SELECT count(*) FROM orders WHERE id=999999;" 1 "the local row is present"
case_end
