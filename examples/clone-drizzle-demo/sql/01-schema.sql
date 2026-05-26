-- Challenging schema for the clone demo. Exercises:
--   * uuid-ossp  -> uuid_generate_v4() column defaults
--   * hstore     -> products.attributes + GIN index, audit diffs
--   * pg_trgm    -> GIN trigram index for fuzzy name search
--   * composite primary keys (product_tags, order_items) -> tests the
--     composite-key path of the GFS overlay clone
--   * an IDENTITY column (audit_log) written by a plpgsql trigger
--
-- Every table has a primary key, which the overlay clone requires to build an
-- updatable view; a table without a usable key would simply be skipped.

CREATE TABLE IF NOT EXISTS tenants (
  id         uuid PRIMARY KEY DEFAULT uuid_generate_v4(),
  slug       text NOT NULL UNIQUE,
  name       text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS products (
  id          uuid PRIMARY KEY DEFAULT uuid_generate_v4(),
  tenant_id   uuid NOT NULL REFERENCES tenants(id),
  sku         text NOT NULL,
  name        text NOT NULL,
  description text,
  attributes  hstore NOT NULL DEFAULT ''::hstore,
  price_cents integer NOT NULL DEFAULT 0,
  created_at  timestamptz NOT NULL DEFAULT now(),
  updated_at  timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX IF NOT EXISTS products_tenant_sku_uq ON products (tenant_id, sku);
-- Fuzzy search on the product name (pg_trgm).
CREATE INDEX IF NOT EXISTS products_name_trgm ON products USING gin (name gin_trgm_ops);
-- Containment / key-existence queries on attributes (hstore).
CREATE INDEX IF NOT EXISTS products_attrs_gin ON products USING gin (attributes);

-- Composite PK: tests composite-key overlay generation in the clone.
CREATE TABLE IF NOT EXISTS product_tags (
  product_id uuid NOT NULL REFERENCES products(id) ON DELETE CASCADE,
  tag        text NOT NULL,
  PRIMARY KEY (product_id, tag)
);

CREATE TABLE IF NOT EXISTS orders (
  id          uuid PRIMARY KEY DEFAULT uuid_generate_v4(),
  tenant_id   uuid NOT NULL REFERENCES tenants(id),
  status      text NOT NULL DEFAULT 'pending',
  total_cents integer NOT NULL DEFAULT 0,
  placed_at   timestamptz NOT NULL DEFAULT now()
);

-- Another composite PK.
CREATE TABLE IF NOT EXISTS order_items (
  order_id         uuid NOT NULL REFERENCES orders(id) ON DELETE CASCADE,
  product_id       uuid NOT NULL REFERENCES products(id),
  qty              integer NOT NULL CHECK (qty > 0),
  unit_price_cents integer NOT NULL,
  PRIMARY KEY (order_id, product_id)
);

-- IDENTITY PK, populated by the plpgsql trigger in 02-triggers.sql.
CREATE TABLE IF NOT EXISTS audit_log (
  id         bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  table_name text NOT NULL,
  op         text NOT NULL,
  row_id     uuid,
  changes    hstore,
  at         timestamptz NOT NULL DEFAULT now()
);
