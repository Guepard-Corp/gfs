#!/usr/bin/env bash
# D1 (#130): you and the source edited DIFFERENT rows. In principle mergeable,
# but divergence is tracked per TABLE (gfs.clone_source.has_local_writes), not
# per row, so GFS cannot prove the two edits do not overlap.
#
# It does not pretend otherwise. The bar this path holds it to is AWARENESS, not
# merging: your row survives, both sync verbs name the table as a conflict and
# say WHY, `gfs pull` refuses to touch it rather than silently picking a side,
# and `gfs status` keeps counting it afterwards. Row-level provenance (the basis
# for a real three-way merge) is a separate project -- see #130 and RFC 007.
. "$(dirname "$0")/../lib/common.sh"
case_begin D1 "disjoint edits are reported as a conflict, by name, and never silently merged"
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null                   # cache the table
q "UPDATE orders SET total=111 WHERE id=1;" >/dev/null 2>&1     # you touch row 1
src "UPDATE orders SET total=222 WHERE id=3;"                   # source touches row 3
nudge

assert_query_eq "SELECT total FROM orders WHERE id=1;" 111 "your row is intact (correctness holds)"

# 1. fetch: the conflict is named, and the reason is the honest one
FET=$("$BIN" fetch --check 2>&1)
assert_match "$FET" "conflict"     "gfs fetch reports the divergence as a conflict"
assert_match "$FET" "orders"       "gfs fetch names the table it cannot resolve on its own"
assert_match "$FET" "local writes" "gfs fetch says why: local writes AND a source change"

# 2. pull: refuses the table instead of choosing a winner, and says how to override
OUT=$("$BIN" pull 2>&1)
assert_match "$OUT" "conflict"     "gfs pull reports the conflict too"
assert_match "$OUT" "NOT touched"  "gfs pull leaves the diverged table alone"
assert_match "$OUT" "pull --force" "gfs pull points at the only way to resolve it"
assert_query_eq "SELECT total FROM orders WHERE id=1;" 111 "your row is still yours after the pull"

# 3. and the conflict does not evaporate: a skipped table stays counted (#120),
#    so `gfs status` alone is enough to know a conflict is outstanding.
JS=$("$BIN" status --output json 2>&1)
assert_match "$JS" '"diverged": *[1-9]' "gfs status still counts the diverged table after the pull skipped it"
case_end
