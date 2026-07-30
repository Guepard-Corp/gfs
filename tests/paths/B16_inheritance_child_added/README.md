# B16: an inheritance child added on the source is adopted under the same parent

**Related issues:** [#108](https://github.com/Guepard-Corp/gfs/issues/108), [#127](https://github.com/Guepard-Corp/gfs/issues/127), [#139](https://github.com/Guepard-Corp/gfs/issues/139)

## Why this test exists

B16 (#127): the source adds an INHERITS child to a parent the clone has.

Worse than a partition, because SELECT ... FROM ONLY parent cannot be federated
at all (#108), so there is no federated escape hatch: correctness requires
actually holding the child's rows.

The children here carry their OWN primary key on purpose. Postgres does not
propagate a parent's PK through INHERITS, and a keyless child cannot be
registered for copy-on-read at all, which currently makes the whole clone
refuse to build (#139, filed separately).

## The scenario

**The source starts with:**

```
a custom schema, created inline by this test (see the script)
```

**Then the source does:**

```sql
CREATE TABLE kid2(PRIMARY KEY (id), CHECK (id > 200)) INHERITS (base);
     INSERT INTO kid2 VALUES (201,'kid2row');
```

## What is asserted

- the parent read includes the existing child
- FROM ONLY excludes the child (this shape cannot federate, #108)
- after pull the parent read includes the NEW child
- FROM ONLY still excludes children after adoption

## Running it

```bash
tests/paths/run-all.sh B16          # through the runner
bash tests/paths/B16_inheritance_child_added/test.sh   # directly
```

Each test builds its **own** throwaway source and its **own** clone, so it can be
run alone and cannot be affected by any other test.

## Harness helpers used

| helper | what it does |
| --- | --- |
| `P` | runs SQL directly inside the clone container |
| `clone_now` | clones the source and waits until the clone is queryable |
| `nudge` | reads an unrelated table so a background drift check can run and commit |
| `src` | runs SQL on the SOURCE database |

See [`../lib/common.sh`](../lib/common.sh) for the harness, and
[`../README.md`](../README.md) for the traps that have produced false results here.
