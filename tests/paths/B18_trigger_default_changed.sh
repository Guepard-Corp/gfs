#!/usr/bin/env bash
# B18: a trigger, default or function changed on the source. Declared
# KNOWN-OPEN: none of these are part of any digest, so the change is invisible.
. "$(dirname "$0")/lib/common.sh"
case_begin B18 "a changed default on the source is not detected" --expect open
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null
src "ALTER TABLE orders ALTER COLUMN total SET DEFAULT 42;"
nudge
OUT=$("$BIN" fetch --check 2>&1)
printf '%s' "$OUT" | grep -qiE "orders" \
  && ok "the default change is detected" \
  || no "the default change is invisible (defaults are in no digest)"
case_end
