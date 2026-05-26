-- Overlay setup on the GFS database (db: gfs).
-- Correctness comes from a UNION ALL view: each row is served from the local
-- store if present (or tombstoned), otherwise from the remote. No partitions,
-- no double counting. Hydration becomes a pure optimisation (warm the cache).

\set ON_ERROR_STOP on

CREATE EXTENSION IF NOT EXISTS postgres_fdw;

DROP SERVER IF EXISTS gfs_remote_srv CASCADE;
CREATE SERVER gfs_remote_srv FOREIGN DATA WRAPPER postgres_fdw
  OPTIONS (host 'remote', port '5432', dbname 'shop');
CREATE USER MAPPING FOR CURRENT_USER SERVER gfs_remote_srv
  OPTIONS (user 'gfs_reader', password 'readerpw');

DROP SCHEMA IF EXISTS gfs_remote CASCADE;
CREATE SCHEMA gfs_remote;
IMPORT FOREIGN SCHEMA public LIMIT TO (orders) FROM SERVER gfs_remote_srv INTO gfs_remote;

-- Local authoritative store (hydrated + written rows) and delete tombstones.
DROP VIEW IF EXISTS orders;
DROP TABLE IF EXISTS orders_local;
DROP TABLE IF EXISTS orders_deleted;
CREATE TABLE orders_local (LIKE gfs_remote.orders INCLUDING DEFAULTS);
ALTER TABLE orders_local ADD PRIMARY KEY (id);
CREATE TABLE orders_deleted (id bigint PRIMARY KEY);

-- Overlay view: local wins; remote rows only if neither local nor tombstoned.
CREATE VIEW orders AS
  SELECT * FROM orders_local
  UNION ALL
  SELECT r.* FROM gfs_remote.orders r
   WHERE NOT EXISTS (SELECT 1 FROM orders_local   l WHERE l.id = r.id)
     AND NOT EXISTS (SELECT 1 FROM orders_deleted t WHERE t.id = r.id);

-- INSTEAD OF triggers: route writes to the local store (copy-on-write).
CREATE OR REPLACE FUNCTION orders_ins() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO orders_local (id, customer, amount, created_at)
    VALUES (NEW.id, NEW.customer, NEW.amount, NEW.created_at)
    ON CONFLICT (id) DO UPDATE
      SET customer = EXCLUDED.customer, amount = EXCLUDED.amount, created_at = EXCLUDED.created_at;
  DELETE FROM orders_deleted WHERE id = NEW.id;  -- re-insert clears a tombstone
  RETURN NEW;
END $$;
CREATE TRIGGER orders_ins INSTEAD OF INSERT ON orders FOR EACH ROW EXECUTE FUNCTION orders_ins();

CREATE OR REPLACE FUNCTION orders_upd() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  -- Copy-up: the (modified) row must live locally afterwards.
  INSERT INTO orders_local (id, customer, amount, created_at)
    VALUES (NEW.id, NEW.customer, NEW.amount, NEW.created_at)
    ON CONFLICT (id) DO UPDATE
      SET customer = EXCLUDED.customer, amount = EXCLUDED.amount, created_at = EXCLUDED.created_at;
  IF NEW.id <> OLD.id THEN  -- key changed: hide the old remote identity
    DELETE FROM orders_local WHERE id = OLD.id;
    INSERT INTO orders_deleted(id) VALUES (OLD.id) ON CONFLICT DO NOTHING;
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER orders_upd INSTEAD OF UPDATE ON orders FOR EACH ROW EXECUTE FUNCTION orders_upd();

CREATE OR REPLACE FUNCTION orders_del() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  DELETE FROM orders_local WHERE id = OLD.id;
  INSERT INTO orders_deleted(id) VALUES (OLD.id) ON CONFLICT DO NOTHING;
  RETURN OLD;
END $$;
CREATE TRIGGER orders_del INSTEAD OF DELETE ON orders FOR EACH ROW EXECUTE FUNCTION orders_del();
