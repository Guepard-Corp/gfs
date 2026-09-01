#!/usr/bin/env bash
# B16b (#139): a source using table inheritance must be cloneable even when the
# child has no unique key of its own.
#
# PostgreSQL does not pass a parent's PRIMARY KEY down to a child created with
# INHERITS, so `CREATE TABLE kid() INHERITS(base)` -- the ordinary way people use
# inheritance -- leaves the child with no index. Copy-on-read needs a unique key
# to fetch a query's rows and to dedupe what it already holds, so such a child
# could not be registered, the #106 safeguard fired, and the WHOLE clone aborted.
# There was no partial result to work with: an everyday schema was simply not
# cloneable.
#
# The fix copies an unkeyed table whole at clone time instead. A wholesale copy
# needs no key. What this test pins down is that the copy is COMPLETE and not
# DOUBLED: an inheritance child is reached both directly and through its parent,
# so a copy that forgets ONLY silently duplicates every child row into the
# parent's heap -- which is #108 reappearing at clone time.
. "$(dirname "$0")/../lib/common.sh"
case_begin B16b "a keyless INHERITS child does not block the clone"

fixture_sql "CREATE TABLE base(id int PRIMARY KEY, v text);
             INSERT INTO base VALUES (1,'base1'),(2,'base2');
             CREATE TABLE kid1(CHECK (id > 100)) INHERITS (base);
             INSERT INTO kid1 VALUES (101,'kid1row'),(102,'kid1row2');"

# Before the fix this aborted the run: the clone was never created.
clone_now

# The child's own rows, read directly. Zero here is the silent-empty-heap failure
# the #106 safeguard existed to prevent.
assert_query_eq "SELECT count(*) FROM ONLY kid1;" 2 "the keyless child holds its own rows"

# The parent expands to include the child. 4 = 2 base + 2 kid1. Six would mean
# the child's rows were copied into the parent's heap as well (#108's shape).
assert_query_eq "SELECT count(*) FROM base;" 4 "parent expands to 4, not doubled"
assert_query_eq "SELECT count(*) FROM ONLY base;" 2 "the parent's own heap holds only its own rows"

# A value from the child, to prove the rows are real rather than a count that
# happens to match.
assert_query_eq "SELECT v FROM base WHERE id = 101;" "kid1row" "a child row is readable through the parent"

# It must be marked fully materialized, or the router would try to lazily fetch
# it -- which is exactly what it cannot do without a key.
V=$(clone_state kid1 whole_cached)
assert_eq "$(tf "$V")" "t" "the child is registered as whole_cached"

case_end
