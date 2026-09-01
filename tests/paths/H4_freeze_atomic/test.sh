#!/usr/bin/env bash
# H4 (#132): freeze is ONE transaction. Killed mid-flight, EVERYTHING rolls
# back -- the truncates, the copies, the bookkeeping and the frozen flag -- and
# the clone is exactly the working lazy clone it was. The freeze backend is
# found by application_name ('gfs_freeze', set by gfs.freeze_run itself), and
# held mid-flight deterministically by an ACCESS EXCLUSIVE lock it must wait on.
. "$(dirname "$0")/../lib/common.sh"
case_begin H4 "a freeze killed mid-flight rolls back completely; the clone stays usable"
fixture_bulk; clone_now
wait_until_cached orders 45 || no "setup: orders never became whole-cached"
C0=$(val "SELECT count(*) FROM orders;")

# hold orders so the freeze blocks at its TRUNCATE, provably mid-transaction
docker exec -d -e PGAPPNAME=h4locker "$CID" \
  psql -U postgres -c "BEGIN; LOCK TABLE public.orders IN ACCESS EXCLUSIVE MODE; SELECT pg_sleep(120);" >/dev/null 2>&1
sleep 1

( "$BIN" freeze > "$WORK/freeze.log" 2>&1 ) & FZ=$!
SEEN=""
for i in $(seq 1 20); do
  [ "$(P "SELECT count(*) FROM pg_stat_activity WHERE application_name='gfs_freeze';")" = "1" ] && { SEEN=yes; break; }
  sleep 1
done
assert_eq "$SEEN" "yes" "the freeze backend is visible mid-flight (application_name=gfs_freeze)"

P "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE application_name='gfs_freeze';" >/dev/null 2>&1
wait "$FZ"; RC=$?
P "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE application_name='h4locker';" >/dev/null 2>&1
[ "$RC" -ne 0 ] && ok "the killed freeze exited non-zero (rc=$RC)" || no "the killed freeze claimed success"

assert_false "$(P "SELECT frozen::text FROM gfs.clone_mode;")" "the frozen flag rolled back"
assert_true  "$(clone_state orders whole_cached)" "coverage bookkeeping rolled back (orders still whole-cached)"
assert_query_eq "SELECT count(*) FROM orders;" "$C0" "no rows were lost to the aborted truncate"

freeze_now || { echo "    freeze log:"; freeze_log | tail -6 | sed 's/^/      /'; }
assert_true "$(P "SELECT frozen::text FROM gfs.clone_mode;")" "a later freeze succeeds cleanly"
V=$(with_source_down "SELECT count(*) FROM orders;")
assert_eq "$V" "$C0" "and the frozen clone answers offline"
case_end
