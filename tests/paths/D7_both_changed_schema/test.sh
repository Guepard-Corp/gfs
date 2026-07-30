#!/usr/bin/env bash
# D7: both sides changed the schema. The local column must not be destroyed.
. "$(dirname "$0")/../lib/common.sh"
case_begin D7 "both sides changed the schema: local columns are preserved"
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null
q "ALTER TABLE orders ADD COLUMN mine text;" >/dev/null 2>&1
src "ALTER TABLE orders ADD COLUMN theirs text;"
nudge
"$BIN" pull >/dev/null 2>&1
COLS=$(P "SELECT string_agg(attname,',' ORDER BY attnum) FROM pg_attribute WHERE attrelid='public.orders'::regclass AND attnum>0 AND NOT attisdropped;")
assert_match "$COLS" "mine" "the locally added column survived"
case_end
