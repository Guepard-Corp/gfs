#!/usr/bin/env bash
# B6 (#122): dropping a column is DESTRUCTIVE, so it must never be applied
# automatically. The local column has to survive.
. "$(dirname "$0")/../lib/common.sh"
case_begin B6 "source DROP COLUMN is a conflict, and the local column survives"
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null
src "ALTER TABLE orders DROP COLUMN total;"
nudge
OUT=$(q "SELECT * FROM orders;")
assert_match   "$OUT" "gfs: the source schema" "clear gfs error"
assert_nomatch "$OUT" 'column .*total.* does not exist' "no raw remote error leaked through"
PULL=$("$BIN" pull 2>&1)
assert_match "$PULL" "conflict" "pull reports a conflict"
COLS=$(P "SELECT string_agg(attname,',' ORDER BY attnum) FROM pg_attribute WHERE attrelid='public.orders'::regclass AND attnum>0 AND NOT attisdropped;")
assert_eq "$COLS" "id,customer,total" "the local column was NOT dropped"
case_end
