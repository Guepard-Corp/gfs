#!/usr/bin/env bash
# E2 (#131): the same tear, inside a single table.
#
# Two halves of one table are observed at different moments. A point-in-time
# clone would show both halves as of ONE instant. Declared KNOWN-OPEN.
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
