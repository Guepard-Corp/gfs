#!/usr/bin/env bash
# B15 (#127): the source adds a PARTITION to a table the clone already has.
#
# This is the QUIET failure. A new partition is reached through a parent the
# clone already serves, so the query SUCCEEDS and simply returns fewer rows than
# the source holds. Nothing errors, nothing warns.
#
# Also guards the false positive the fix exposed: gfs.source_map is built from
# clone_source JOIN pg_foreign_table, so a partitioned PARENT (relkind='p', never
# registered because it stores no rows) can never appear in it. Testing "new" as
# "absent from source_map" reported the parent as a brand new table forever, and
# because new_table COUNTS AS ATTRIBUTION that phantom switched off the
# unattributed blanket entirely, masking real drift.
. "$(dirname "$0")/../lib/common.sh"
case_begin B15 "a partition added on the source is adopted, and the parent is not reported as new"
fixture_sql "CREATE TABLE ev(id int, ts date, note text, PRIMARY KEY (id, ts)) PARTITION BY RANGE (ts);
             CREATE TABLE ev_2024 PARTITION OF ev FOR VALUES FROM ('2024-01-01') TO ('2025-01-01');
             CREATE TABLE ev_2025 PARTITION OF ev FOR VALUES FROM ('2025-01-01') TO ('2026-01-01');
             INSERT INTO ev VALUES (1,'2024-03-01','a'),(2,'2024-06-01','b'),(3,'2025-03-01','c');"
clone_now

assert_query_eq "SELECT count(*) FROM ev;" 3 "the partitioned table reads through the parent"
LEAF=$(P "SELECT count(*) FROM gfs.source_map m JOIN pg_class c ON c.oid=m.relid WHERE c.relname LIKE 'ev\_%';")
assert_eq "$LEAF" "2" "both leaf partitions are registered for copy-on-read"
PAR=$(P "SELECT count(*) FROM gfs.source_map m JOIN pg_class c ON c.oid=m.relid WHERE c.relkind='p';")
assert_eq "$PAR" "0" "the partitioned parent is deliberately NOT registered (it stores no rows)"

src "CREATE TABLE ev_2026 PARTITION OF ev FOR VALUES FROM ('2026-01-01') TO ('2027-01-01');
     INSERT INTO ev VALUES (4,'2026-02-01','new'),(5,'2026-05-01','new2');"
nudge

OUT=$("$BIN" fetch --check 2>&1)
assert_match "$OUT" "ev_2026" "the new partition is named by fetch --check"
# the phantom: the parent must NOT be called a new table
if printf '%s' "$OUT" | grep -E "new table|created on the source" | grep -qE "public\.ev([^_]|$)"; then
  no "PHANTOM: the partitioned parent public.ev is reported as a new table"
else
  ok "the partitioned parent is NOT reported as new"
fi
assert_eq "$(P "SELECT count(*) FROM gfs.drift_notes WHERE kind='new_table' AND subject='public.ev';")" "0" \
          "no phantom new_table note for the parent"

"$BIN" pull >/dev/null 2>&1
assert_query_eq "SELECT count(*) FROM ev;" 5 "after pull the clone has every row, including the new partition"
assert_eq "$(P "SELECT count(*) FROM gfs.source_map m JOIN pg_class c ON c.oid=m.relid WHERE c.relname='ev_2026';")" "1" \
          "the adopted partition is registered for copy-on-read"
assert_eq "$(P "SELECT count(*) FROM gfs.source_table_baseline b JOIN pg_class c ON c.oid=b.relid WHERE c.relname='ev_2026';")" "1" \
          "it has a drift baseline, so it will be re-checked like any other table"
# an adopted table that is never re-checked would be adopted once and then go stale forever
src "INSERT INTO ev VALUES (6,'2026-08-01','later');"
nudge
assert_query_eq "SELECT count(*) FROM ev;" 6 "a LATER write into the adopted partition is picked up too"
case_end
