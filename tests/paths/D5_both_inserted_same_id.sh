#!/usr/bin/env bash
# D5: both sides inserted the same id with different data. There is no correct
# automatic merge, so the local row must win and the clash must be reported.
. "$(dirname "$0")/lib/common.sh"
case_begin D5 "both sides inserted the same id: the local row wins, loudly"
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null
q "INSERT INTO orders VALUES (42,'MineLocal',1);" >/dev/null 2>&1
src "INSERT INTO orders VALUES (42,'TheirsRemote',2);"
nudge
assert_query_eq "SELECT customer FROM orders WHERE id=42;" "MineLocal" "the local row wins"
assert_match "$("$BIN" pull 2>&1)" "conflict" "the clash is reported"
assert_query_eq "SELECT customer FROM orders WHERE id=42;" "MineLocal" "and the local row is still there"
case_end
