#!/usr/bin/env bash
# D8: you have local data, the source drops a column. Dropping it locally would
# destroy your rows' contents, so it must be refused.
. "$(dirname "$0")/../lib/common.sh"
case_begin D8 "your data plus a source column drop: the column is not dropped locally"
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null
q "INSERT INTO orders VALUES (99,'Mine',777);" >/dev/null 2>&1
src "ALTER TABLE orders DROP COLUMN total;"
nudge
assert_match "$("$BIN" pull 2>&1)" "conflict" "reported as a conflict"
COLS=$(P "SELECT string_agg(attname,',' ORDER BY attnum) FROM pg_attribute WHERE attrelid='public.orders'::regclass AND attnum>0 AND NOT attisdropped;")
assert_match "$COLS" "total" "the local column, holding your data, is preserved"
case_end
