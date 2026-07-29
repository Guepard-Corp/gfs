#!/usr/bin/env bash
# B4b (#119): after drifting, a table must return to the LOCAL path rather than
# federating forever. Before the fix it federated on every read, permanently,
# which degrades the clone into a pass-through proxy.
#
# Proof that it is genuinely local again: it answers with the source STOPPED.
# The whole-own is completed by the background worker, so wait for the state to
# actually flip rather than sleeping a guessed number of seconds.
. "$(dirname "$0")/lib/common.sh"
case_begin B4b "a drifted table self-heals back onto the local path"
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null
src "INSERT INTO orders VALUES (4,'Dave',40);"
nudge
assert_query_eq "SELECT count(*) FROM orders;" 4 "correct while federating"

"$BIN" pull >/dev/null 2>&1
if wait_until_cached orders 45; then
  ok "the table was re-copied locally after the pull (whole_cached flipped back)"
  V=$(with_source_down "SELECT count(*) FROM orders;")
  assert_eq "$V" "4" "answered with the source DOWN, so it truly healed to local"
else
  no "the table never became whole-cached again within 45s: it is still federating (#119)"
fi
case_end
