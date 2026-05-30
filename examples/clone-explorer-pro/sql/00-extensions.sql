-- pg_trgm powers the fuzzy search and its GIN indexes on the SOURCE. The clone's
-- local store is created with LIKE (no indexes), so a fuzzy search on the clone
-- is a local Seq Scan even once fully cached — a deliberate teaching point about
-- "the clone serves it locally, but indexes are an app concern".
CREATE EXTENSION IF NOT EXISTS pg_trgm;
-- gen_random_uuid() is in core since PG13; no extension needed for customers.id.
