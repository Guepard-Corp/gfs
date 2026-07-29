#!/usr/bin/env bash
# G2: with the source unreachable and the table NEVER copied, there is nothing
# to answer from.
#
# The FEATURE gap (no offline mode) is real and still open, but the BEHAVIOUR is
# correct today: it fails loudly instead of inventing an answer. So this asserts
# the current, correct behaviour and is expected to pass. Read the description,
# not the status, for the gap.
. "$(dirname "$0")/lib/common.sh"
case_begin G2 "an uncopied table fails loudly offline, it does not fabricate an answer"
fixture_simple; clone_now
assert_eq "$(local_bytes public.orders)" "0" "the table really has not been copied"
OUT=$(with_source_down "SELECT count(*) FROM orders;")
assert_nomatch "$OUT" "^[0-9]+$" "no number is returned when the data is not held and the source is gone"
assert_match   "$OUT" "error|could not connect|unreachable|failed" "it reports a failure explicitly"
case_end
