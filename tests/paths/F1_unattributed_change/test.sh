#!/usr/bin/env bash
# F1 (#129): a change nothing can account for must NOT be hidden just because
# some other table has an explainable change. Attribution is load-bearing.
. "$(dirname "$0")/../lib/common.sh"
case_begin F1 "a fresh clone reports no drift, and the verdict is honest"
fixture_simple; clone_now
assert_eq "$(P 'SELECT count(*) FROM gfs.source_drift();' | grep -v WARNING | tail -1)" "0" \
          "an untouched source produces zero findings"
src "INSERT INTO orders VALUES (4,'Dave',40);"
nudge
D=$(P "SELECT count(*) FROM gfs.drift_state WHERE drifted;")
[ "${D:-0}" -ge 1 ] && ok "the real change is attributed to a table ($D drifted)" \
                    || no "the source changed but no table is marked drifted (got '$D')"
case_end
