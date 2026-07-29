#!/usr/bin/env bash
# D1 (#130): you and the source edited DIFFERENT rows. In principle mergeable,
# but divergence is tracked per TABLE, so this is indistinguishable from a real
# conflict. Declared KNOWN-OPEN. Correctness is fine; the limitation is that a
# mergeable case is treated as a conflict.
. "$(dirname "$0")/lib/common.sh"
case_begin D1 "disjoint edits are reported as a conflict because tracking is per table" --expect open
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null
q "UPDATE orders SET total=111 WHERE id=1;" >/dev/null 2>&1     # you touch row 1
src "UPDATE orders SET total=222 WHERE id=3;"                    # source touches row 3
nudge
assert_query_eq "SELECT total FROM orders WHERE id=1;" 111 "your row is intact (correctness holds)"
OUT=$("$BIN" pull 2>&1)
printf '%s' "$OUT" | grep -qi "conflict" \
  && no "reported as a conflict even though the edits are disjoint and mergeable (#130)" \
  || ok "the disjoint edits were merged rather than treated as a conflict"
case_end
