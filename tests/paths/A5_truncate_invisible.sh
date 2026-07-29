#!/usr/bin/env bash
# A5 / B4 (#117): TRUNCATE moves no write counter, so it used to look like
# "nothing changed". Only n_live_tup reveals it.
. "$(dirname "$0")/lib/common.sh"
case_begin A5 "source TRUNCATE is detected (write counters alone cannot see it)"
fixture_simple; clone_now
val "SELECT count(*) FROM orders;" >/dev/null
W_BEFORE=$(srcq "SELECT n_tup_ins||'/'||n_tup_upd||'/'||n_tup_del FROM pg_stat_user_tables WHERE relname='orders'")
src "TRUNCATE orders;"
sleep 2
W_AFTER=$(srcq "SELECT n_tup_ins||'/'||n_tup_upd||'/'||n_tup_del FROM pg_stat_user_tables WHERE relname='orders'")
assert_eq "$W_AFTER" "$W_BEFORE" "the write counters really are unchanged by TRUNCATE (ins/upd/del)"
nudge
assert_query_eq "SELECT count(*) FROM orders;" 0 "the clone still reports the table as empty"
case_end
