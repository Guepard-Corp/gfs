#!/usr/bin/env bash
# H1 (#132, closes #131): two tables copied at different moments are TORN -- the
# clone holds a combination that never existed at the source. `gfs freeze` must
# end that: one snapshot, every table from the same instant, and the answer
# stops changing.
#
# The tear is reproduced first (orphan payments > 0), then frozen away. The
# drift verdict is deliberately kept stale (check_interval = 1 hour) so the
# lazy copies actually go out of step, as they do between background checks.
. "$(dirname "$0")/../lib/common.sh"
case_begin H1 "freeze ends the tear: every table from one instant, answers stop moving"
fixture_sql "CREATE TABLE orders(id int PRIMARY KEY, customer text, total int);
             INSERT INTO orders VALUES (1,'Alice',50),(2,'Bob',30),(3,'Carol',20);
             CREATE TABLE payments(id int PRIMARY KEY, order_id int);
             INSERT INTO payments VALUES (1,1),(2,2),(3,3);"
clone_now
P "UPDATE gfs.sync_policy SET check_interval='1 hour';" >/dev/null 2>&1

val "SELECT count(*) FROM orders;" >/dev/null       # orders copied NOW (3 rows)
sleep 3                                             # let the first drift check commit
src "INSERT INTO orders VALUES (4,'Dave',40); INSERT INTO payments VALUES (4,4);"
val "SELECT count(*) FROM payments;" >/dev/null     # payments copied LATER (4 rows)

ORPHANS="SELECT count(*) FROM payments p LEFT JOIN orders o ON p.order_id = o.id WHERE o.id IS NULL;"
assert_query_eq "$ORPHANS" 1 "the tear is real first: an orphan payment for an order the clone lacks (#131)"

freeze_now || { echo "    freeze log:"; freeze_log | tail -6 | sed 's/^/      /'; }

assert_query_eq "$ORPHANS" 0 "freeze ended the tear: no orphan payments"
assert_query_eq "SELECT count(*) FROM orders;" 4 "orders re-copied from the freeze instant"
assert_query_eq "SELECT count(*) FROM payments;" 4 "payments re-copied from the same instant"

src "INSERT INTO orders VALUES (5,'Eve',10); INSERT INTO payments VALUES (5,5);"
nudge
assert_query_eq "SELECT count(*) FROM orders;" 4 "the frozen clone holds still while the source moves on"
assert_query_eq "$ORPHANS" 0 "and stays internally consistent"
case_end
