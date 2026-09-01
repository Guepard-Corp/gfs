#!/usr/bin/env bash
# D6: you emptied the table, the source inserted. Refilling from the source
# would undo the emptying you asked for.
. "$(dirname "$0")/../lib/common.sh"
case_begin D6 "you emptied, the source inserted: the table stays empty"
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null
q "DELETE FROM orders;" >/dev/null 2>&1
src "INSERT INTO orders VALUES (4,'Dave',40);"
nudge
assert_query_eq "SELECT count(*) FROM orders;" 0 "the clone stays empty"
assert_match "$("$BIN" pull 2>&1)" "conflict" "reported rather than silently refilled"
case_end
