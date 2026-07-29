#!/usr/bin/env bash
# B13 (#125): nothing READS a sequence, so no counter moves and no drift is
# detected. The clone's counter falls behind ids the source already issued.
. "$(dirname "$0")/lib/common.sh"
case_begin B13 "source sequence advancing does not cause a duplicate key on the clone"
fixture_sql "CREATE TABLE items(id serial PRIMARY KEY, v text); INSERT INTO items(v) SELECT 'x'||g FROM generate_series(1,3) g;"
clone_now
val "SELECT count(*) FROM items;" >/dev/null
src "INSERT INTO items(v) SELECT 'y'||g FROM generate_series(1,20) g;"
nudge
"$BIN" pull >/dev/null 2>&1
val "SELECT count(*) FROM items;" >/dev/null
OUT=$(q "INSERT INTO items(v) VALUES ('local');")
assert_nomatch "$OUT" "duplicate key" "a local insert does not collide with fetched ids"
case_end
