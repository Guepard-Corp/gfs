#!/usr/bin/env bash
# C5: you change the schema on the clone. Your column is yours, and a later
# source-side repair must not remove it.
. "$(dirname "$0")/../lib/common.sh"
case_begin C5 "a locally added column survives source activity and a pull"
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null
q "ALTER TABLE orders ADD COLUMN mine text;" >/dev/null 2>&1
src "INSERT INTO orders VALUES (4,'Dave',40);"
nudge
"$BIN" pull >/dev/null 2>&1
COLS=$(P "SELECT string_agg(attname,',' ORDER BY attnum) FROM pg_attribute WHERE attrelid='public.orders'::regclass AND attnum>0 AND NOT attisdropped;")
assert_match "$COLS" "mine" "the locally added column is still there after a pull"
case_end
