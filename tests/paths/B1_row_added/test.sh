#!/usr/bin/env bash
# B1 (#118): the source adds a row after the clone cached the table.
. "$(dirname "$0")/../lib/common.sh"
case_begin B1 "source INSERT is visible to the clone"
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null      # cache it
src "INSERT INTO orders VALUES (4,'Dave',40);"
nudge
# #133: while the drift verdict is live (before the read below heals it),
# `status --output json` must carry a source section. Shape, not counts:
# autoheal races make exact behind/diverged values timing-dependent.
"$BIN" fetch --check >/dev/null 2>&1
SJ=$("$BIN" status --output json 2>&1)
assert_match "$SJ" '"source"' "status JSON includes the source section"
assert_match "$SJ" '"tracked": *[1-9]' "status JSON counts tracked tables"
assert_match "$SJ" '"behind": *[0-9]' "status JSON reports a behind count"
assert_match "$SJ" '"last_checked"' "status JSON carries the verdict timestamp"
assert_query_eq "SELECT count(*) FROM orders;" 4 "returns the new count, not the cached 3"
case_end
