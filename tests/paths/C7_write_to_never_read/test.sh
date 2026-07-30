#!/usr/bin/env bash
# C7: writing to a table that was never read must first make the local copy
# complete, so the write lands on full data rather than an empty heap.
. "$(dirname "$0")/../lib/common.sh"
case_begin C7 "writing to a never-read table copies it first"
fixture_simple; clone_now
assert_eq "$(local_bytes public.orders)" "0" "nothing copied before the write"
q "INSERT INTO orders VALUES (99,'Mine',1);" >/dev/null 2>&1
sleep 2
assert_query_eq "SELECT count(*) FROM orders;" 4 "the 3 source rows plus the local one"
case_end
