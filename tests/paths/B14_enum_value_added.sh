#!/usr/bin/env bash
# B14 (#126): a label added upstream makes the table UNREADABLE, not merely
# stale: the clone's copy of the type cannot represent the fetched value.
. "$(dirname "$0")/lib/common.sh"
case_begin B14 "an enum label added on the source is replicated in the source's order"
fixture_sql "CREATE TYPE mood AS ENUM ('happy','sad');
             CREATE TABLE people(id int PRIMARY KEY, name text, m mood);
             INSERT INTO people VALUES (1,'ann','happy'),(2,'bob','sad');"
clone_now
val "SELECT count(*) FROM people;" >/dev/null
src "ALTER TYPE mood ADD VALUE 'excited' AFTER 'happy';"
src "INSERT INTO people VALUES (3,'cy','excited');"
nudge
"$BIN" pull >/dev/null 2>&1
L=$(P "SELECT string_agg(enumlabel,',' ORDER BY enumsortorder) FROM pg_enum e JOIN pg_type t ON t.oid=e.enumtypid WHERE t.typname='mood';")
assert_eq "$L" "happy,excited,sad" "the label is inserted in the SOURCE's order"
assert_query_eq "SELECT count(*) FROM people;" 3 "the table is readable again"
case_end
