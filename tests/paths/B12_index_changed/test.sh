#!/usr/bin/env bash
# B12: an index added or dropped upstream. This is a SPEED-only difference: it
# must never change the answers, and must never be treated as a conflict.
. "$(dirname "$0")/../lib/common.sh"
case_begin B12 "an index change on the source does not affect correctness"
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null
src "CREATE INDEX idx_orders_customer ON orders(customer);"
nudge
assert_query_eq "SELECT count(*) FROM orders;" 3 "answers are unchanged by an index"
OUT=$("$BIN" pull 2>&1)
assert_nomatch "$OUT" "conflict" "an index change is not reported as a conflict"
case_end
