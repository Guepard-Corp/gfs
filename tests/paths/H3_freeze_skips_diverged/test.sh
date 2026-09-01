#!/usr/bin/env bash
# H3 (#132): a table with LOCAL writes is the user's branch. Freeze must NOT
# re-copy it (that would clobber the user's work), must say so, and the frozen
# clone still serves it locally -- "the source as of freeze time, plus my
# changes". The skip predicate is gfs.relation_diverged_sql, verbatim.
. "$(dirname "$0")/../lib/common.sh"
case_begin H3 "freeze keeps a diverged table (and says so), re-copies the rest"
fixture_simple; clone_now
val "SELECT count(*) FROM notes;" >/dev/null        # copy notes, then diverge it
q "INSERT INTO notes VALUES (2,'local');" >/dev/null 2>&1
assert_true "$(P "SELECT gfs.relation_diverged_sql(cs.relid)::text FROM gfs.clone_source cs JOIN gfs.source_map m ON m.relid=cs.relid WHERE m.src_table='notes';")" \
  "notes is diverged (local write recorded)"

src "UPDATE orders SET total=99 WHERE id=1; UPDATE notes SET body='upstream' WHERE id=1;"

freeze_now || { echo "    freeze log:"; freeze_log | tail -6 | sed 's/^/      /'; }
assert_match "$(freeze_log)" "kept" "freeze reports the kept table"
assert_match "$(freeze_log)" "local writes" "and says WHY it was kept"

assert_query_eq "SELECT count(*) FROM notes;" 2 "the local insert survived the freeze"
assert_query_eq "SELECT body FROM notes WHERE id=1;" "n1" "the source's change to the kept table was NOT taken"
assert_query_eq "SELECT total FROM orders WHERE id=1;" 99 "a non-diverged table WAS re-copied from the freeze instant"

V=$(with_source_down "SELECT count(*) FROM notes;")
assert_eq "$V" "2" "the kept table answers with the source stopped (router serves it locally)"
case_end
