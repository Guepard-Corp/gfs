#!/usr/bin/env bash
# E1 (#131): tables are copied at different moments, so a clone can hold a
# combination of rows that never existed on the source at any single instant.
# Declared KNOWN-OPEN: there is no point-in-time guarantee.
. "$(dirname "$0")/lib/common.sh"
case_begin E1 "a clone can mix data from different moments across tables" --expect open
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null          # orders captured at T1
src "INSERT INTO orders VALUES (4,'Dave',40); INSERT INTO notes VALUES (2,'n2');"
sleep 2
val "SELECT count(*) FROM notes;" >/dev/null           # notes captured at T2
O=$(val "SELECT count(*) FROM orders;"); N=$(val "SELECT count(*) FROM notes;")
echo "    (orders=$O notes=$N; the source has orders=4 notes=2)"
# A point-in-time clone would show either (3,1) or (4,2), never a mix.
if { [ "$O" = "3" ] && [ "$N" = "1" ]; } || { [ "$O" = "4" ] && [ "$N" = "2" ]; }; then
  ok "the two tables agree on a single moment"
else
  no "the clone shows orders=$O with notes=$N, a combination the source never had (#131)"
fi
case_end
