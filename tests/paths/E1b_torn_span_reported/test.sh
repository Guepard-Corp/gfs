#!/usr/bin/env bash
# E1b (#131): tornness is a COMPUTABLE FACT now, not a silent property. Every
# copy event stamps gfs.copy_watermark with where the source was when the rows
# arrived, and gfs.clone_moments() folds the stamps into a clone-level verdict
# with NO source contact.
#
# Two facts are pinned, in order:
#   * tables copied while the source held still are ONE moment -- benign WAL
#     movement between the two copies must not count (the F1b lesson applied
#     to #131: moment identity is the row-activity totals, not the LSN);
#   * a table copied at a LATER moment makes the clone torn, and both
#     `gfs fetch` and `gfs status` say so, with the remedy.
#
# Getting the second copy takes a `gfs pull` first, and that is not a detail:
# once the source moves, the drift guard flags the affected tables and reads of
# them FEDERATE (correct, and the whole point of #118/#119) -- a federated read
# copies nothing, so it stamps nothing. `pull` is what puts a flagged table back
# on the lazy path, so the next read is a real copy event at the later moment.
# `notes` is never touched by the source, so it keeps its moment-A stamp and the
# two moments are what the verdict compares.
#
# The orphan-payments JOIN itself (the issue's worked example) lives in H1,
# which also proves `gfs freeze` ends it; this is the detection-side complement.
. "$(dirname "$0")/../lib/common.sh"
case_begin E1b "a torn clone reports the moments it spans; a coherent one reports one"
fixture_sql "CREATE TABLE orders(id int PRIMARY KEY, customer text, total int);
             INSERT INTO orders VALUES (1,'Alice',50),(2,'Bob',30),(3,'Carol',20);
             CREATE TABLE payments(id int PRIMARY KEY, order_id int);
             INSERT INTO payments VALUES (1,1),(2,2),(3,3);
             CREATE TABLE notes(id int PRIMARY KEY, body text);
             INSERT INTO notes VALUES (1,'n1');"
clone_now

val "SELECT count(*) FROM orders;" >/dev/null      # copied at moment A
val "SELECT count(*) FROM notes;"  >/dev/null      # copied while the source held still
assert_false "$(P "SELECT torn FROM gfs.clone_moments();")" \
  "two tables copied while the source held still are ONE moment (not torn)"
assert_eq "$(P "SELECT count(*)::text FROM gfs.copy_watermark;")" "2" \
  "and both copies really were stamped (a silent no-stamp would also read as 'not torn')"

src "INSERT INTO orders VALUES (4,'Dave',40); INSERT INTO payments VALUES (4,4);"
sleep 2                                            # let the source's stats flush
"$BIN" pull >/dev/null 2>&1                        # flagged tables back on the lazy path
val "SELECT count(*) FROM payments;" >/dev/null    # NOW it copies: moment B
wait_until_cached payments

assert_true "$(P "SELECT torn FROM gfs.clone_moments();")" \
  "a table copied at a later moment makes the clone torn"
MOMENTS=$(P "SELECT moment_count FROM gfs.clone_moments();")
[ "${MOMENTS:-0}" -ge 2 ] 2>/dev/null \
  && ok "the verdict counts at least two source moments ($MOMENTS)" \
  || no "expected moment_count >= 2, got '$MOMENTS'"

FETCH=$("$BIN" fetch 2>&1)
assert_match "$FETCH" "spans .*moments" "gfs fetch reports the span"
assert_match "$FETCH" "gfs freeze" "and points at the remedy"
STATUS=$("$BIN" status 2>&1)
assert_match "$STATUS" "Moments" "gfs status shows the Moments row"
assert_match "$STATUS" "torn" "and marks it torn"
case_end
