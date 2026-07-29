#!/usr/bin/env bash
# E2 (#131): a single table copied in pieces can be torn INSIDE itself, holding
# part of one moment and part of another. Declared KNOWN-OPEN.
. "$(dirname "$0")/lib/common.sh"
case_begin E2 "a partly copied table can be torn inside itself" --expect open
fixture_bulk; clone_now
val "SELECT count(*) FROM orders WHERE id BETWEEN 1 AND 100;" >/dev/null
src "UPDATE orders SET total=0 WHERE id BETWEEN 1 AND 100;"
sleep 2
val "SELECT count(*) FROM orders WHERE id BETWEEN 4900 AND 5000;" >/dev/null
Z=$(val "SELECT count(*) FROM orders WHERE total=0;")
echo "    (rows with total=0 on the clone: $Z; the source has 100)"
[ "$Z" = "100" ] && ok "the table reflects one consistent moment" \
                 || no "the table mixes moments inside itself (got $Z of 100) (#131)"
case_end
