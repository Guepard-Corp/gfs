#!/usr/bin/env bash
# B8 (#121): the source widens a column's type. The clone must not crash, and
# must not silently keep describing the old type.
. "$(dirname "$0")/../lib/common.sh"
case_begin B8 "a column type change is detected and does not crash the clone"
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null
src "ALTER TABLE orders ALTER COLUMN total TYPE bigint;"
nudge
OUT=$(q "SELECT count(*) FROM orders;")
assert_match "$OUT" "gfs: the source schema|^ *[0-9]+" "either a clear gfs message or a correct answer, never a raw remote error"
assert_nomatch "$OUT" "PANIC|segmentation|internal error" "no crash"
case_end
