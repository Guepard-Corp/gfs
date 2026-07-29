#!/usr/bin/env bash
# B10 (#123): the source drops a whole table, leaving an orphaned foreign table.
. "$(dirname "$0")/lib/common.sh"
case_begin B10 "a table dropped on the source is reported clearly"
fixture_sql "CREATE TABLE goner(id int PRIMARY KEY, v text); INSERT INTO goner VALUES (1,'x');"
clone_now
val "SELECT count(*) FROM goner;" >/dev/null
src "DROP TABLE goner;"
nudge
assert_match "$(q 'SELECT * FROM goner;')" "gfs:|no longer exists|does not exist" "the drop is reported"
assert_match "$("$BIN" pull 2>&1)" "conflict|no longer exists|orphan" "pull mentions the orphaned table"
case_end
