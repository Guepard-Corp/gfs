# PoC: row-level read-through cache — INCONCLUSIVE (measurement flawed)

**Requirement under test.** Each source row read once → cached locally → every
later read served locally, with zero source contact, at **row** granularity
(never bulk-copy a big table).

## Status: the measurement is unreliable — do not trust the verdict line

`run.sh` (single container, postgres_fdw loopback) reliably shows the
**write-through itself works**: a join touching 200 orders caches exactly the
200 referenced customers out of 100,000 — no bulk copy, result equals source.

But the key question — *does a SECOND read of the same join still contact the
source for the now-cached rows?* — is **not** answered correctly here:

- The script counts source contact from `log_statement=all` lines matching
  `statement:`. But `postgres_fdw` issues its remote queries over the
  **extended query protocol**, logged as `execute …` / `parse …`, **not**
  `statement:`. So the counter misses the real FDW round-trips.
- Result: the script printed "ROUND 2 src.customers reads = 0 / pure SQL
  suffices", yet the ROUND 2 plan still contains
  `Foreign Scan on rmt.customers → Remote SQL: SELECT id, name FROM src.customers`
  (a full-table remote read). The "0" is almost certainly a measurement
  artifact, not a real zero.

**Conclusion: unknown.** This PoC must be re-done with a correct measurement
before drawing any architectural conclusion.

## How to measure it correctly (next iteration)

- Put the source in a **separate container** so its log is isolated, and count
  *all* query kinds (`statement:`, `parse`, `bind`, `execute`), or
- read `pg_stat_user_tables.seq_scan / idx_scan` (or `pg_stat_statements`) on the
  source before/after ROUND 2, or
- use `EXPLAIN (ANALYZE)` and inspect actual rows fetched by the foreign scan.

The question to settle: **at the second run of a join, does the SQL overlay
re-contact the source for rows already cached locally?**
- If **yes** → a custom local-first scan (the `gfs_clone` extension) is required.
- If **no** → row-caching already works in pure SQL, and the extension only adds
  *transparency* (caching without an explicit warm call).

## Run

```bash
bash run.sh   # ~30-60s; one container; cleans up on exit. Verdict NOT trustworthy yet.
```
