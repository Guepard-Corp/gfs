#!/usr/bin/env bash
# C8: you consume id numbers on the clone. Those ids are yours; the source must
# not be affected, and the clone must keep issuing usable ids.
. "$(dirname "$0")/../lib/common.sh"
case_begin C8 "ids consumed on the clone do not affect the source"
fixture_sql "CREATE TABLE items(id serial PRIMARY KEY, v text); INSERT INTO items(v) SELECT 'x'||g FROM generate_series(1,3) g;"
clone_now
W0=$(src_write_counter)
val "SELECT count(*) FROM items;" >/dev/null
q "INSERT INTO items(v) VALUES ('mine1');" >/dev/null 2>&1
q "INSERT INTO items(v) VALUES ('mine2');" >/dev/null 2>&1
assert_query_eq "SELECT count(*) FROM items WHERE v LIKE 'mine%';" 2 "both local inserts landed"
assert_src_eq "SELECT count(*) FROM items" "3" "the source still has only its own rows"
assert_source_untouched "$W0"
case_end
