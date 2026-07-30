# B4b: a drifted table self-heals back onto the local path

**Related issues:** [#119](https://github.com/Guepard-Corp/gfs/issues/119)

## Why this test exists

B4b (#119): after drifting, a table must return to the LOCAL path rather than
federating forever. Before the fix it federated on every read, permanently,
which degrades the clone into a pass-through proxy.

Proof that it is genuinely local again: it answers with the source STOPPED.
The whole-own is completed by the background worker, so wait for the state to
actually flip rather than sleeping a guessed number of seconds.

## The scenario

**The source starts with:**

```
orders(id, customer, total) with 3 rows: Alice 50, Bob 30, Carol 20
notes(id, body) with 1 row
other(id, v) with 1 row, used only to trigger background checks
```

**Then the source does:**

```sql
INSERT INTO orders VALUES (4,'Dave',40);
```

## What is asserted

- correct while federating
- answered with the source DOWN, so it truly healed to local
- the table was re-copied locally after the pull (whole_cached flipped back)
- the table never became whole-cached again within 45s: it is still federating (#119)

## Running it

```bash
tests/paths/run-all.sh B4b          # through the runner
bash tests/paths/B4b_never_self_healed/test.sh   # directly
```

Each test builds its **own** throwaway source and its **own** clone, so it can be
run alone and cannot be affected by any other test.

## Harness helpers used

| helper | what it does |
| --- | --- |
| `clone_now` | clones the source and waits until the clone is queryable |
| `nudge` | reads an unrelated table so a background drift check can run and commit |
| `src` | runs SQL on the SOURCE database |
| `val` | reads through the gfs CLI and returns one value |
| `wait_until_cached` | polls until the table is genuinely held whole locally |
| `with_source_down` | evaluates a query with the source stopped, then restarts it |

See [`../lib/common.sh`](../lib/common.sh) for the harness, and
[`../README.md`](../README.md) for the traps that have produced false results here.
