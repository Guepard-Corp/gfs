#!/usr/bin/env bash
# C2: you update a row on the clone. It must persist and must never reach the source.
. "$(dirname "$0")/../lib/common.sh"
case_begin C2 "a local UPDATE persists and never reaches the source"
fixture_simple; clone_now
W0=$(src_write_counter)
val "SELECT count(*) FROM orders;" >/dev/null
q "UPDATE orders SET total=555 WHERE id=1;" >/dev/null 2>&1
assert_query_eq "SELECT total FROM orders WHERE id=1;" 555 "the local value is set"
assert_src_eq "SELECT total FROM orders WHERE id=1" "50" "the source is unchanged"
assert_source_untouched "$W0"
case_end
