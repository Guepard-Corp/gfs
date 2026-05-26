-- Extensions required by the demo schema. Run before the schema so that the
-- hstore type and uuid_generate_v4() default resolve at CREATE TABLE time.
--
-- The matching set is what `SELECT extname FROM pg_extension;` reports on the
-- source. GFS mirrors these onto the clone (best-effort) during bootstrap.
CREATE EXTENSION IF NOT EXISTS "uuid-ossp";
CREATE EXTENSION IF NOT EXISTS hstore;
CREATE EXTENSION IF NOT EXISTS pg_trgm;
CREATE EXTENSION IF NOT EXISTS pg_stat_statements;
CREATE EXTENSION IF NOT EXISTS dblink;
-- plpgsql is installed by default; listed for parity with the source.
CREATE EXTENSION IF NOT EXISTS plpgsql;
