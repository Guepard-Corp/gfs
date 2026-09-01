#!/usr/bin/env bash
# E2b (#131): the within-table variant. An int-range key hydrates KEY RANGES at
# different moments into ONE table; the per-table min..max watermark plus a
# moment count make that reportable ("rows copied across N moments") -- the
# thing a single per-table timestamp could never express. Which ROW came from
# which moment stays unknowable (ranges coalesce, and ON CONFLICT DO NOTHING
# keeps older row versions silently), so the span is the honest ceiling, and
# this test pins exactly that ceiling.
#
# The source is moved by writing to a DIFFERENT table (`other`), which is the
# only way to get two copy moments INSIDE one table: writing to `orders` itself
# would flag it, and a flagged table federates rather than copies, so the second
# read would stamp nothing at all (see E1b). Moving `other` advances the
# source-wide totals -- the moment identity -- while leaving `orders` on the
# lazy path, so its second chunk is a genuine copy event at a later moment.
. "$(dirname "$0")/../lib/common.sh"
case_begin E2b "one table copied chunk-by-chunk reports the moments its rows span"
fixture_bulk; clone_now
P "UPDATE gfs.sync_policy SET check_interval='1 hour';" >/dev/null 2>&1

val "SELECT count(*) FROM orders WHERE id BETWEEN 1 AND 100;" >/dev/null      # range chunk, moment A
echo "    (after chunk A: whole_cached=$(clone_state orders whole_cached), ranges=$(P "SELECT count(*) FROM gfs.cached c JOIN gfs.source_map sm ON sm.relid=c.relid WHERE sm.src_table='orders';"))"
src "INSERT INTO other VALUES (2,'b');"                                       # source moves, orders untouched
sleep 2                                                                        # let the source's stats flush
val "SELECT count(*) FROM orders WHERE id BETWEEN 4000 AND 4100;" >/dev/null   # range chunk, moment B

M=$(P "SELECT w.moments FROM gfs.copy_watermark w JOIN gfs.source_map sm ON sm.relid=w.relid WHERE sm.src_table='orders';")
[ "${M:-0}" -ge 2 ] 2>/dev/null \
  && ok "the table's own watermark counts both copy moments ($M)" \
  || no "expected moments >= 2 on orders, got '$M'"
assert_true "$(P "SELECT torn FROM gfs.clone_moments();")" \
  "a within-table tear makes the whole clone torn"
SPAN=$(P "SELECT format('%s..%s', w.first_lsn, w.last_lsn) FROM gfs.copy_watermark w JOIN gfs.source_map sm ON sm.relid=w.relid WHERE sm.src_table='orders';")
RANGES=$(P "SELECT count(*) FROM gfs.cached c JOIN gfs.source_map sm ON sm.relid=c.relid WHERE sm.src_table='orders';")
echo "    (orders spans $SPAN across $RANGES cached key range(s) -- per-row provenance is gone once ranges coalesce; the span is the ceiling)"
FETCH=$("$BIN" fetch 2>&1)
assert_match "$FETCH" "spans .*moments" "gfs fetch reports the span"
case_end
