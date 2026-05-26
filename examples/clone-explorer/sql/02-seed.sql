-- Seeds :rows products and a baseline of orders on the SOURCE only.
-- Server-side generate_series → seeding 500k rows takes a few seconds.
INSERT INTO products (name, sku, category, price_cents, attrs)
SELECT
  'Product ' || g || ' '
    || (ARRAY['alpha','bravo','crimson','delta','echo','falcon','garnet','hazel'])[1 + (g % 8)],
  'SKU-' || lpad(g::text, 9, '0'),
  (ARRAY['books','games','tools','garden','audio','kitchen'])[1 + (g % 6)],
  ((g * 37) % 90000) + 100,
  jsonb_build_object('w', (g % 50) + 1, 'color', (ARRAY['red','blue','green'])[1 + (g % 3)])
FROM generate_series(1, :rows) g;

-- A few baseline orders so both panels start with identical order history.
INSERT INTO orders (product_id, qty, unit_cents, origin)
SELECT g, 1 + (g % 3), ((g * 37) % 90000) + 100, 'source'
FROM generate_series(1, 100) g;
