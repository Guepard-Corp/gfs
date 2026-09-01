#!/usr/bin/env bash
# B18: a trigger, default or function changed on the source. Declared
# KNOWN-OPEN: none of these are part of any digest, so the change is invisible
# AS A SHAPE CHANGE.
#
# Careful with the assertion here. Any DDL moves the source's WAL, and if nothing
# accounts for that movement the unattributed blanket marks EVERY table suspect
# (see #140). So merely finding the table's name in `fetch --check` proves
# nothing: it is the blanket talking, not default detection. The precise question
# is whether the change is recognised as a SHAPE change, which is what
# schema_drifted records.
. "$(dirname "$0")/../lib/common.sh"
case_begin B18 "a changed default on the source is not recognised as a schema change" --expect open
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null
src "ALTER TABLE orders ALTER COLUMN total SET DEFAULT 42;"
nudge
SD=$(P "SELECT d.schema_drifted::text FROM gfs.drift_state d JOIN gfs.source_map m ON m.relid=d.relid WHERE m.src_table='orders';")
echo "    (schema_drifted for orders = '$SD'; defaults are in no digest)"
[ "$(tf "$SD")" = "t" ] \
  && ok "the default change IS recognised as a schema change" \
  || no "the default change is invisible as a shape change (defaults are in no digest)"
# and the local default really is still the old one
DEF=$(P "SELECT COALESCE(pg_get_expr(d.adbin,d.adrelid),'none') FROM pg_attribute a
         LEFT JOIN pg_attrdef d ON d.adrelid=a.attrelid AND d.adnum=a.attnum
         WHERE a.attrelid='public.orders'::regclass AND a.attname='total';")
echo "    (local default is '$DEF'; the source now has 42)"
case_end
