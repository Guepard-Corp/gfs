#!/usr/bin/env bash
# D9: you changed the schema locally, the source changed data. These do not
# actually conflict, but the local schema must survive whatever repair happens.
. "$(dirname "$0")/lib/common.sh"
case_begin D9 "your schema plus source data: the local schema survives"
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null
q "ALTER TABLE orders ADD COLUMN note text;" >/dev/null 2>&1
src "UPDATE orders SET total=888 WHERE id=1;"
nudge
"$BIN" pull >/dev/null 2>&1
COLS=$(P "SELECT string_agg(attname,',' ORDER BY attnum) FROM pg_attribute WHERE attrelid='public.orders'::regclass AND attnum>0 AND NOT attisdropped;")
assert_match "$COLS" "note" "the locally added column survived"
case_end
