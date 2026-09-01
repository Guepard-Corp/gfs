#!/usr/bin/env bash
# B16 (#127): the source adds an INHERITS child to a parent the clone has.
#
# Worse than a partition, because SELECT ... FROM ONLY parent cannot be federated
# at all (#108), so there is no federated escape hatch: correctness requires
# actually holding the child's rows.
#
# The children here carry their OWN primary key on purpose. Postgres does not
# propagate a parent's PK through INHERITS, and a keyless child cannot be
# registered for copy-on-read at all, which currently makes the whole clone
# refuse to build (#139, filed separately).
. "$(dirname "$0")/../lib/common.sh"
case_begin B16 "an inheritance child added on the source is adopted under the same parent"
fixture_sql "CREATE TABLE base(id int PRIMARY KEY, v text);
             INSERT INTO base VALUES (1,'base1');
             CREATE TABLE kid1(PRIMARY KEY (id), CHECK (id > 100)) INHERITS (base);
             INSERT INTO kid1 VALUES (101,'kid1row');"
clone_now

assert_query_eq "SELECT count(*) FROM base;" 2 "the parent read includes the existing child"
assert_query_eq "SELECT count(*) FROM ONLY base;" 1 "FROM ONLY excludes the child (this shape cannot federate, #108)"

src "CREATE TABLE kid2(PRIMARY KEY (id), CHECK (id > 200)) INHERITS (base);
     INSERT INTO kid2 VALUES (201,'kid2row');"
nudge
"$BIN" pull >/dev/null 2>&1

assert_query_eq "SELECT count(*) FROM base;" 3 "after pull the parent read includes the NEW child"
assert_eq "$(P "SELECT count(*) FROM gfs.source_map m JOIN pg_class c ON c.oid=m.relid WHERE c.relname='kid2';")" "1" \
          "the adopted child is registered for copy-on-read"
# INHERITS does not carry the parent's key down, so adoption must add one itself
assert_eq "$(P "SELECT count(*) FROM pg_index i JOIN pg_class c ON c.oid=i.indrelid WHERE c.relname='kid2' AND i.indisunique;")" "1" \
          "the adopted child was given its own unique key (INHERITS does not inherit one)"
assert_query_eq "SELECT count(*) FROM ONLY base;" 1 "FROM ONLY still excludes children after adoption"
case_end
