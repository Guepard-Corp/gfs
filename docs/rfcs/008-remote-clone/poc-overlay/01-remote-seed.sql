-- Runs on the REMOTE database (db: shop, user: postgres).
-- Seeds a table with a bigint PK and a read-only role for GFS,
-- demonstrating the "read-only strict" assumption (SELECT only).

DO $$
BEGIN
  IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'gfs_reader') THEN
    CREATE ROLE gfs_reader LOGIN PASSWORD 'readerpw';
  END IF;
END
$$;

CREATE TABLE IF NOT EXISTS orders (
  id         bigint PRIMARY KEY,
  customer   text NOT NULL,
  amount     numeric(10,2) NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now()
);

TRUNCATE orders;

INSERT INTO orders (id, customer, amount, created_at)
SELECT g,
       'cust_' || (g % 1000),
       (g % 500) + (g % 100) / 100.0,
       now() - (g || ' minutes')::interval
FROM generate_series(1, 30000) AS g;

-- Read-only strict: gfs_reader may only SELECT.
GRANT USAGE ON SCHEMA public TO gfs_reader;
GRANT SELECT ON ALL TABLES IN SCHEMA public TO gfs_reader;
