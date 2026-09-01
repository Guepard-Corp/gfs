#!/usr/bin/env bash
# H5 (#132): freezing copies EVERYTHING, so it must refuse a copy bigger than
# the byte budget BEFORE moving any data (the honest-limitation guard: a
# 556M-row source is real, and copying it by accident would be catastrophic).
# --force overrides deliberately.
. "$(dirname "$0")/../lib/common.sh"
case_begin H5 "freeze refuses above the copy budget without copying anything; --force overrides"
fixture_bulk; clone_now
W0=$(src_write_counter)

freeze_now --max-bytes 1000
RC=$?
[ "$RC" -ne 0 ] && ok "freeze refused (rc=$RC) under a 1000-byte budget" || no "freeze did not refuse"
assert_match "$(freeze_log)" "exceeds" "the refusal names the budget"
assert_match "$(freeze_log)" "copies everything" "and states the honest limitation"
assert_eq "$(local_bytes public.orders)" "0" "NOTHING was copied: the never-read table still has zero local bytes"
assert_false "$(P "SELECT frozen::text FROM gfs.clone_mode;")" "the clone is not marked frozen"
assert_source_untouched "$W0"

freeze_now --max-bytes 1000 --force || { echo "    freeze log:"; freeze_log | tail -6 | sed 's/^/      /'; }
assert_true "$(P "SELECT frozen::text FROM gfs.clone_mode;")" "--force takes the snapshot regardless"
V=$(with_source_down "SELECT count(*) FROM orders;")
assert_eq "$V" "5000" "the forced snapshot answers offline"
case_end
