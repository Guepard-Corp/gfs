#!/usr/bin/env bash
# F1c (#140/#129): the guard on the #140 fix.
#
# #140 stops the blanket from firing when no row changed anywhere on the source.
# That reasoning is only sound while the source's row counters actually count. A
# real marketplace source measured during this work reported zero live tuples
# across all 138 of its tables while `order_items` alone held 556 million rows,
# with track_counts=on and stats_reset=never -- its stats file had been lost.
#
# On such a source every check looks quiet, so "quiet" must NOT be read as "no
# change": here a real INSERT moves no counter at all, and only the blanket is
# left to catch it. Absence of evidence is not evidence of quiet.
. "$(dirname "$0")/../lib/common.sh"
case_begin F1c "a source whose counters do not count still gets the blanket"
# Scoped explicitly: `VAR=v func` leaking past the call is shell-dependent.
GFS_SOURCE_ARGS="-c track_counts=off"
fixture_simple
unset GFS_SOURCE_ARGS
clone_now
val "SELECT count(*) FROM orders;" >/dev/null

# Premise: confirm the counters really are dead, or the case proves nothing.
src "INSERT INTO orders VALUES (4,'Dave',40);"
sleep 2
W=$(srcq "SELECT COALESCE(sum(n_tup_ins+n_tup_upd+n_tup_del),0) FROM pg_stat_user_tables;")
L=$(srcq "SELECT COALESCE(sum(n_live_tup),0) FROM pg_stat_user_tables;")
if [ "$W" != "0" ] || [ "$L" != "0" ]; then
  sk "this source's counters still count (writes=$W live=$L), so it is not the case under test"
else
  ok "a real INSERT moved no counter anywhere on the source (writes=$W live=$L)"
  nudge
  assert_eq "$(P "SELECT count(*) FROM gfs.source_drift() WHERE kind='unattributed';" | grep -v WARNING | tail -1)" "1" \
            "the movement is blanketed rather than read as quiet"
  D=$(P "SELECT count(*) FROM gfs.drift_state WHERE drifted;")
  [ "${D:-0}" -ge 1 ] && ok "every copied table is marked suspect ($D drifted)" \
                      || no "no table was marked suspect (got '$D'): a real change would be served stale"
  # The payoff, and what regresses if the guard is dropped: read the new row.
  assert_query_eq "SELECT count(*) FROM orders;" 4 "so the row that moved no counter is still seen"

  # Teeth. Everything above would also pass if the blanket fired for some
  # unrelated reason, so evaluate the bare comparison the guard wraps: on this
  # source it reads FALSE -- "no row changed anywhere" -- while a row demonstrably
  # did. That false reading is precisely what the guard exists to override.
  WITHOUT=$(P "SELECT (b.tot_writes IS DISTINCT FROM (p->'totals'->>'w')::bigint
                    OR b.tot_live   IS DISTINCT FROM (p->'totals'->>'l')::bigint)::text
                 FROM gfs.source_baseline b,
                      LATERAL (SELECT gfs.source_probe() AS p) x;")
  assert_eq "$(tf "$WITHOUT")" "f" "and unguarded, that same comparison calls the source quiet"
fi
case_end
