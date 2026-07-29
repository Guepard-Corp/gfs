#!/usr/bin/env bash
# F2: "unchanged" is only true as of the last check. This is a real detection
# limit, not a bug: between checks the clone can be confidently out of date.
. "$(dirname "$0")/lib/common.sh"
case_begin F2 "the unchanged verdict has a window, and it closes on the next check"
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null
P "UPDATE gfs.sync_policy SET check_interval='1 hour';" >/dev/null 2>&1
src "INSERT INTO orders VALUES (4,'Dave',40);"
sleep 3
V=$(val "SELECT count(*) FROM orders;")
echo "    (inside the window the clone answers '$V'; the source has 4)"
P "UPDATE gfs.sync_policy SET check_interval='1 second';" >/dev/null 2>&1
nudge
assert_query_eq "SELECT count(*) FROM orders;" 4 "once a check runs, the verdict is corrected"
case_end
