#!/usr/bin/env bash
# E2 (#131): the same tear, inside a single table.
#
# Two halves of one table are observed at different moments. A point-in-time
# clone would show both halves as of ONE instant. Declared KNOWN-OPEN, and it
# stays that way on purpose: a LAZY clone cannot fix this -- the source has
# already thrown the old row versions away -- so this path keeps asserting what
# actually happens rather than what we wish did.
#
# What IS fixed is the SILENCE. E2b proves the table's own watermark counts the
# moments its rows span, and H8 proves `gfs freeze` (#132) collapses the clone
# back to a single moment. This path is the tear; those two are the awareness
# and the cure.
. "$(dirname "$0")/../lib/common.sh"
case_begin E2 "one table can reflect two different moments" --expect open
fixture_bulk; clone_now
LOW1=$(val "SELECT count(*) FROM orders WHERE id BETWEEN 1 AND 100;")
src "INSERT INTO orders SELECT g,'late'||g,g FROM generate_series(9000,9100) g;"
nudge
LOW2=$(val "SELECT count(*) FROM orders WHERE id BETWEEN 1 AND 100;")
TOT=$(val "SELECT count(*) FROM orders;")
echo "    (low slice: '$LOW1' then '$LOW2'; total now '$TOT', source grew mid-run)"
[ "$TOT" = "5000" ] \
  && ok "the table still reflects the moment it was first observed" \
  || no "the table now reflects a later moment ($TOT rows) than the one first read (#131, fixed by #132)"
case_end
