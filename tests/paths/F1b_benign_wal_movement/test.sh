#!/usr/bin/env bash
# F1b (#140): an idle PostgreSQL advances its own WAL -- checkpointer, autovacuum,
# the stats collector. That movement is indistinguishable from a real change by
# LSN alone, so it used to fire the unattributed blanket and mark EVERY copied
# table suspect. The router computes `stale = drifted && !diverged`, so a
# blanket-marked table cannot be served locally even when the rows are held: the
# clone federates every read and offline reads stop working. That is #119's
# failure reached through a different door.
#
# The blanket itself must stay -- #129 exists because it was once suppressed too
# eagerly. What separates the two cases is whether any row actually changed
# anywhere on the source, which F1c pins from the other side.
. "$(dirname "$0")/../lib/common.sh"
case_begin F1b "housekeeping WAL movement does not mark every copied table suspect"
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null   # settle a baseline before measuring

BEFORE=$(srcq "SELECT pg_current_wal_lsn();")
src "CHECKPOINT;"                               # forced: always writes a WAL record
AFTER=$(srcq "SELECT pg_current_wal_lsn();")

if [ "$AFTER" = "$BEFORE" ]; then
  # Without movement every assertion below passes for the wrong reason.
  sk "the source's WAL did not move, so there is nothing to misread (still $BEFORE)"
else
  ok "the source's WAL moved with no user write ($BEFORE -> $AFTER)"
  nudge
  assert_eq "$(P 'SELECT count(*) FROM gfs.source_drift();' | grep -v WARNING | tail -1)" "1" \
            "the movement is reported, not swallowed"
  assert_eq "$(P "SELECT count(*) FROM gfs.source_drift() WHERE kind='unattributed';" | grep -v WARNING | tail -1)" "0" \
            "but it is not the blanket: no table is called suspect"
  assert_eq "$(P 'SELECT count(*) FROM gfs.drift_state WHERE drifted;')" "0" \
            "so no table is marked drifted by housekeeping alone"
  # The point of not blanketing: the rows are held, so they must be servable.
  assert_query_eq "SELECT count(*) FROM orders;" 3 "the clone still answers from its own copy"
fi
case_end
