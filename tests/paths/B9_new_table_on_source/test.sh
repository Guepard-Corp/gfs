#!/usr/bin/env bash
# B9 (#124): a brand new standalone table on the source. It is NOT adopted, by
# design: reproducing arbitrary DDL (indexes, defaults, triggers, grants) is the
# clone bootstrap's job. But it must be REPORTED by name, not silently ignored.
. "$(dirname "$0")/../lib/common.sh"
case_begin B9 "a new standalone table on the source is reported by name"
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null
src "CREATE TABLE brandnew(id int PRIMARY KEY, v text); INSERT INTO brandnew VALUES (1,'x');"
nudge
OUT=$("$BIN" fetch --check 2>&1)
assert_match "$OUT" "brandnew" "the new table is named"
assert_match "$OUT" "re-clone|new table|created on the source" "and it says what to do about it"
# reading it fails loudly, which is the acceptable behaviour for a table we do not have
assert_match "$(q 'SELECT * FROM brandnew;')" "does not exist|gfs:" "reading it fails loudly rather than returning nothing"
case_end
