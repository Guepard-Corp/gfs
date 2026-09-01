# B16b: a keyless INHERITS child does not block the clone

**Related issues:** [#106](https://github.com/Guepard-Corp/gfs/issues/106), [#108](https://github.com/Guepard-Corp/gfs/issues/108), [#139](https://github.com/Guepard-Corp/gfs/issues/139)

## Why this test exists

B16b (#139): a source using table inheritance must be cloneable even when the
child has no unique key of its own.

PostgreSQL does not pass a parent's PRIMARY KEY down to a child created with
INHERITS, so `CREATE TABLE kid() INHERITS(base)` -- the ordinary way people use
inheritance -- leaves the child with no index. Copy-on-read needs a unique key
to fetch a query's rows and to dedupe what it already holds, so such a child
could not be registered, the #106 safeguard fired, and the WHOLE clone aborted.
There was no partial result to work with: an everyday schema was simply not
cloneable.

The fix copies an unkeyed table whole at clone time instead. A wholesale copy
needs no key. What this test pins down is that the copy is COMPLETE and not
DOUBLED: an inheritance child is reached both directly and through its parent,
so a copy that forgets ONLY silently duplicates every child row into the
parent's heap -- which is #108 reappearing at clone time.

## The scenario

**The source starts with:**

```
a custom schema, created inline by this test (see the script)
```

The source is not modified: this test is about what the clone does on its own.

## What is asserted

- the keyless child holds its own rows
- parent expands to 4, not doubled
- the parent's own heap holds only its own rows
- a child row is readable through the parent
- the child is registered as whole_cached

## Running it

```bash
tests/paths/run-all.sh B16b          # through the runner
bash tests/paths/B16b_keyless_inheritance_child/test.sh   # directly
```

Each test builds its **own** throwaway source and its **own** clone, so it can be
run alone and cannot be affected by any other test.

## Harness helpers used

| helper | what it does |
| --- | --- |
| `clone_now` | clones the source and waits until the clone is queryable |
| `clone_state` | reads a column of gfs.clone_source for one table |

See [`../lib/common.sh`](../lib/common.sh) for the harness, and
[`../README.md`](../README.md) for the traps that have produced false results here.
