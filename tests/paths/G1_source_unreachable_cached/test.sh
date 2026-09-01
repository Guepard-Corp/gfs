#!/usr/bin/env bash
# G1: with the source unreachable, an already-copied table must still answer.
# This is the entire promise of holding a local copy.
. "$(dirname "$0")/../lib/common.sh"
case_begin G1 "a copied table still answers when the source is unreachable"
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null
sleep 3
docker stop "$SRC_CT" >/dev/null 2>&1
V=$(val "SELECT count(*) FROM orders;")
docker start "$SRC_CT" >/dev/null 2>&1; sleep 4
assert_eq "$V" "3" "answered offline"
case_end
