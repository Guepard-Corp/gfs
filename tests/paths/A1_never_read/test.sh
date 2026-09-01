#!/usr/bin/env bash
# A1: a table never read cannot be stale. It holds NO rows locally, and the very
# first read must still return exactly what the source has.
#
# Note the ordering below: every local-state assertion happens BEFORE the first
# read. Scanning the table through psql would go through the planner hook and
# hydrate it, destroying the state under test (see local_bytes in common.sh).
. "$(dirname "$0")/../lib/common.sh"
case_begin A1 "a never-read table holds nothing locally but reads correctly"
fixture_simple; clone_now

assert_eq "$(local_bytes public.orders)" "0" "the local heap has zero pages: nothing was copied at clone time"
assert_false "$(clone_state orders whole_cached)" "orders is not marked whole-cached"
assert_eq "$(clone_state orders partial_rows)" "0" "no partial slice has been pulled either"

assert_query_eq "SELECT count(*) FROM orders;" 3 "the first read returns the source's rows"

# #133: `gfs remote` names the source from the local catalog (no probe) and
# must never print the mapped password (the clone URL is postgres://postgres:x@...).
RM=$("$BIN" remote 2>&1)
assert_match   "$RM" "host.docker.internal:$PORT" "remote prints the source host and port"
assert_match   "$RM" "fetch only" "remote states the fetch-only rule"
assert_nomatch "$RM" ":x@" "the source password is never printed"
case_end
