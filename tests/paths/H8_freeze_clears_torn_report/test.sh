#!/usr/bin/env bash
# H8 (#131 meets #132): the detection/freeze handshake. H1 proves freeze ends
# the TEAR (the orphan JOIN empties); H8 proves the REPORT tracks it: torn
# with a span before the freeze, single-moment after -- by construction, not
# by arithmetic (frozen short-circuits gfs.clone_moments(), and freeze_run
# clears each re-copied table's watermark before warm re-stamps it with the
# ONE freeze mark, so the raw table never contradicts the verdict).
#
# Also pins frozen PRECEDENCE in the UX: a frozen clone shows the detached
# output, never the torn/spans line, and status shows no Moments row.
#
# Reaching "torn" needs a real second copy event, which needs a `gfs pull`
# first: a table the drift guard has flagged FEDERATES, and a federated read
# copies -- and therefore stamps -- nothing. See E1b for the same sequence and
# why it is the honest one. `other` is never touched by the source, so it holds
# the moment-A stamp that the moment-B copy is torn against.
. "$(dirname "$0")/../lib/common.sh"
case_begin H8 "freeze clears the torn report: spans-N before, single moment after"
fixture_sql "CREATE TABLE orders(id int PRIMARY KEY, customer text, total int);
             INSERT INTO orders VALUES (1,'Alice',50),(2,'Bob',30),(3,'Carol',20);
             CREATE TABLE payments(id int PRIMARY KEY, order_id int);
             INSERT INTO payments VALUES (1,1),(2,2),(3,3);"
clone_now

val "SELECT count(*) FROM orders;" >/dev/null       # copied at moment A
val "SELECT count(*) FROM other;"  >/dev/null       # ... and stays at moment A
src "INSERT INTO orders VALUES (4,'Dave',40); INSERT INTO payments VALUES (4,4);"
sleep 2                                             # let the source's stats flush
"$BIN" pull >/dev/null 2>&1                         # flagged tables back on the lazy path
val "SELECT count(*) FROM payments;" >/dev/null     # copied at moment B
wait_until_cached payments

assert_true "$(P "SELECT torn FROM gfs.clone_moments();")" "torn before the freeze"
FETCH=$("$BIN" fetch 2>&1)
assert_match "$FETCH" "spans .*moments" "gfs fetch shows the span before the freeze"

freeze_now || { echo "    freeze log:"; freeze_log | tail -6 | sed 's/^/      /'; }

assert_eq "$(P "SELECT state FROM gfs.clone_moments();")" "frozen" "the verdict switches to frozen"
assert_false "$(P "SELECT torn FROM gfs.clone_moments();")" \
  "not torn after the freeze: single moment by construction"
assert_eq "$(P "SELECT moment_count FROM gfs.clone_moments();")" "1" "exactly one moment"
DISTINCT=$(P "SELECT count(DISTINCT w.last_lsn) FROM gfs.copy_watermark w;")
assert_eq "$DISTINCT" "1" "every re-copied table carries the one freeze mark"
assert_true "$(P "SELECT bool_and(w.last_lsn = m.frozen_lsn::pg_lsn) FROM gfs.copy_watermark w CROSS JOIN gfs.clone_mode m;")" \
  "and it IS the recorded freeze mark"

FETCH2=$("$BIN" fetch 2>&1)
assert_nomatch "$FETCH2" "spans .*moments" "the torn line never renders on a frozen clone"
assert_match "$FETCH2" "detached snapshot" "the detached output takes precedence"
STATUS=$("$BIN" status 2>&1)
assert_match "$STATUS" "frozen snapshot" "status shows the frozen branch"
assert_nomatch "$STATUS" "Moments" "and no Moments row"
case_end
