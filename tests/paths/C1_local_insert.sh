#!/usr/bin/env bash
# C1: you insert on the clone. The row must persist, and the SOURCE must never
# see it (the golden rule: gfs never writes upstream).
. "$(dirname "$0")/lib/common.sh"
case_begin C1 "a local INSERT persists and never reaches the source"
fixture_simple; clone_now
W0=$(src_write_counter)
q "INSERT INTO orders VALUES (99,'Mine',1);" >/dev/null 2>&1
assert_query_eq "SELECT count(*) FROM orders WHERE id=99;" 1 "the local row is there"
assert_src_eq "SELECT count(*) FROM orders WHERE id=99" "0" "the source does NOT have the row"
assert_source_untouched "$W0"
case_end
