#!/usr/bin/env bash
# A4: "nothing changed" is only true as of the last check. This documents the
# window: a source write is not visible until a drift check has run.
. "$(dirname "$0")/../lib/common.sh"
case_begin A4 "the unchanged verdict is a snapshot, and refreshes on the next check"
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null
assert_eq "$(P 'SELECT count(*) FROM gfs.source_drift();' | grep -v WARNING | tail -1)" "0" "no drift while nothing has changed"
src "INSERT INTO orders VALUES (4,'Dave',40);"
nudge
assert_query_eq "SELECT count(*) FROM orders;" 4 "once a check has run, the new row is visible"
case_end
