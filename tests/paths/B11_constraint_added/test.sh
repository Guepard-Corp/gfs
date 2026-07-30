#!/usr/bin/env bash
# B11 (#121): a constraint added upstream. Also guards the regression where
# folding constraints into the COLUMN digest made every table mismatch, because
# a foreign table can never carry constraints.
. "$(dirname "$0")/../lib/common.sh"
case_begin B11 "a constraint added on the source is detected and applied"
fixture_simple; clone_now
# regression guard first: on an untouched clone every digest must already agree
DIS=$(P "SELECT count(*) FROM gfs.source_map m JOIN gfs.source_table_baseline b USING (relid) WHERE gfs.relation_fp(m.relid) IS DISTINCT FROM b.src_fp;")
assert_eq "$DIS" "0" "column digests agree on a fresh clone (else everything federates forever)"
val "SELECT count(*) FROM orders;" >/dev/null
src "ALTER TABLE orders ADD CONSTRAINT total_pos CHECK (total > 0);"
nudge
assert_match "$("$BIN" fetch --check 2>&1)" "orders" "the constraint change is detected"
"$BIN" pull >/dev/null 2>&1
HAS=$(P "SELECT count(*) FROM pg_constraint WHERE conrelid='public.orders'::regclass AND pg_get_constraintdef(oid)='CHECK ((total > 0))';")
assert_eq "$HAS" "1" "the constraint is applied locally (matched by definition, not name)"
case_end
