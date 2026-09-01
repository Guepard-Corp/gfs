#!/usr/bin/env bash
# E1 (#131, fixed by #132): a clone used as a BRANCH must hold still. Before
# snapshot mode this was declared KNOWN-OPEN: drift detection kept reads
# current, which is precisely the problem -- the same query changed answers
# over time with no local action, so no test run was repeatable. `gfs freeze`
# is the fix under test: once frozen, repeated reads agree no matter what the
# source does.
. "$(dirname "$0")/../lib/common.sh"
case_begin E1 "a frozen clone holds still: repeated reads agree despite source writes"
fixture_simple; clone_now
FIRST=$(val "SELECT count(*) FROM orders;")
freeze_now || { echo "    freeze log:"; freeze_log | tail -6 | sed 's/^/      /'; }
src "INSERT INTO orders VALUES (4,'Dave',40);"
nudge
SECOND=$(val "SELECT count(*) FROM orders;")
echo "    (same query, no local action: first read '$FIRST', later read '$SECOND')"
assert_eq "$SECOND" "$FIRST" "the clone held still: repeated reads agree, so it behaves as a snapshot (#132)"
case_end
