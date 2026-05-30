-- Richer schema exercising the clone across key types, joins, and aggregates.
--
--   categories   smallint PK, tiny        -> whole_table (small)
--   customers    uuid PK                  -> whole_table (non-rangeable key)
--   products     bigint PK, large         -> range elision + fuzzy -> promotion
--   orders       bigint PK, FK + time     -> temporal filter (range_time: federates)
--   order_items  composite PK (order,line)-> composite-key overlay + generated col
--   reviews      bigint PK, FK, text body -> fuzzy search target
--   events       bigint PK, time-series   -> volume + temporal
--
-- FKs are declared on the SOURCE for realism; the clone's per-table overlays do
-- not enforce cross-table FKs (a data overlay, not a behavioural replica).

-- Idempotent: re-running run.sh reuses the existing source container, so every
-- object is created with IF NOT EXISTS. The seed TRUNCATEs before inserting.

CREATE TABLE IF NOT EXISTS categories (
  id   smallint PRIMARY KEY,
  name text NOT NULL
);

CREATE TABLE IF NOT EXISTS customers (
  id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  n          bigint UNIQUE NOT NULL,           -- dense surrogate, used for seeding/joining
  email      text NOT NULL,
  country    text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS products (
  id          bigint PRIMARY KEY,
  name        text NOT NULL,
  category_id smallint NOT NULL REFERENCES categories(id),
  price_cents int NOT NULL,
  in_stock    boolean NOT NULL DEFAULT true
);
CREATE INDEX IF NOT EXISTS products_category_idx ON products (category_id);
-- Trigram index makes fuzzy search fast ON THE SOURCE; the clone's local store
-- won't have it (indexes aren't cloned) -> fuzzy on the clone is a Seq Scan.
CREATE INDEX IF NOT EXISTS products_name_trgm ON products USING gin (name gin_trgm_ops);

CREATE TABLE IF NOT EXISTS orders (
  id          bigint PRIMARY KEY,
  customer_id uuid NOT NULL REFERENCES customers(id),
  placed_at   timestamptz NOT NULL,
  status      text NOT NULL DEFAULT 'paid'
);
CREATE INDEX IF NOT EXISTS orders_customer_idx ON orders (customer_id);
CREATE INDEX IF NOT EXISTS orders_placed_idx ON orders (placed_at);

CREATE TABLE IF NOT EXISTS order_items (
  order_id    bigint NOT NULL REFERENCES orders(id),
  line        int NOT NULL,
  product_id  bigint NOT NULL REFERENCES products(id),
  qty         int NOT NULL CHECK (qty > 0),
  unit_cents  int NOT NULL,
  total_cents int GENERATED ALWAYS AS (qty * unit_cents) STORED,
  PRIMARY KEY (order_id, line)
);
CREATE INDEX IF NOT EXISTS order_items_product_idx ON order_items (product_id);

CREATE TABLE IF NOT EXISTS reviews (
  id          bigint PRIMARY KEY,
  product_id  bigint NOT NULL REFERENCES products(id),
  customer_id uuid NOT NULL REFERENCES customers(id),
  rating      smallint NOT NULL,
  body        text NOT NULL,
  created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS reviews_product_idx ON reviews (product_id);
CREATE INDEX IF NOT EXISTS reviews_body_trgm ON reviews USING gin (body gin_trgm_ops);

CREATE TABLE IF NOT EXISTS events (
  id         bigint PRIMARY KEY,
  product_id bigint NOT NULL,
  kind       text NOT NULL,
  at         timestamptz NOT NULL
);
CREATE INDEX IF NOT EXISTS events_at_idx ON events (at);
