#!/usr/bin/env bash
# B10b: the source RENAMES a table. Declared KNOWN-OPEN: there is no rename
# detection, so it reads as a drop plus an unrelated new table. This documents
# today's behaviour so a future fix has something to flip.
. "$(dirname "$0")/lib/common.sh"
case_begin B10b "a renamed source table is reported as a drop plus a new table" --expect open
fixture_sql "CREATE TABLE oldname(id int PRIMARY KEY, v text); INSERT INTO oldname VALUES (1,'x');"
clone_now
val "SELECT count(*) FROM oldname;" >/dev/null
src "ALTER TABLE oldname RENAME TO newname;"
nudge
OUT=$("$BIN" fetch --check 2>&1)
assert_match "$OUT" "newname" "the new name is at least reported as a new table"
# the rename is NOT understood as a rename: that is the gap
printf '%s' "$OUT" | grep -qiE "renamed" \
  && ok "the rename is recognised as a rename" \
  || no "the rename is not recognised as such (reads as drop + unrelated new table)"
case_end
