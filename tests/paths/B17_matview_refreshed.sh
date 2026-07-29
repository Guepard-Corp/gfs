#!/usr/bin/env bash
# B17 (#127): the source REFRESHes a materialized view.
#
# A matview on the clone is a LOCAL object computed from the clone's own tables,
# and those tables are copy-on-read. So recomputing it locally is what makes it
# current: nothing has to be copied from the source's stored matview contents.
. "$(dirname "$0")/lib/common.sh"
case_begin B17 "a matview refreshed on the source becomes current after a pull"
fixture_sql "CREATE TABLE prod(id int PRIMARY KEY, price int);
             INSERT INTO prod VALUES (1,10),(2,20);
             CREATE MATERIALIZED VIEW mv_tot AS SELECT count(*) AS n, sum(price) AS total FROM prod;"
clone_now

assert_query_eq "SELECT n FROM mv_tot;" 2 "the matview is populated at clone time (a replayed matview arrives empty)"

src "INSERT INTO prod VALUES (3,30);"
src "REFRESH MATERIALIZED VIEW mv_tot;"
nudge
"$BIN" pull >/dev/null 2>&1

assert_query_eq "SELECT n FROM mv_tot;" 3 "after pull the matview reflects the refresh"
assert_query_eq "SELECT total FROM mv_tot;" 60 "and its aggregate is recomputed, not just the count"
assert_query_eq "SELECT count(*) FROM prod;" 3 "the matview's BASE table is current too"
case_end
