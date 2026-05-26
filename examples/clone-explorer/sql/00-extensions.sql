-- Fuzzy search on product names. The clone must mirror this extension; the
-- search endpoint then works identically against the clone.
CREATE EXTENSION IF NOT EXISTS pg_trgm;
