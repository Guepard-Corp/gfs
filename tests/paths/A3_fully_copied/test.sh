#!/usr/bin/env bash
# A3: once fully copied and with the source unchanged, reads are local and must
# survive the source being unreachable.
. "$(dirname "$0")/../lib/common.sh"
case_begin A3 "a fully copied table is served locally, even with the source down"
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null      # trigger the copy
sleep 3
docker stop "$SRC_CT" >/dev/null 2>&1
V=$(val "SELECT count(*) FROM orders;")
docker start "$SRC_CT" >/dev/null 2>&1; sleep 4
assert_eq "$V" "3" "answered with the source stopped, so it was served locally"
case_end
