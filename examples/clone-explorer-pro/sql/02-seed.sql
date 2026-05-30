-- Set-based seed (psql vars: :products :customers :orders :reviews :events).
-- Referential integrity via modular hash joins (fast even at millions of rows).
\set ON_ERROR_STOP on

TRUNCATE categories, customers, products, orders, order_items, reviews, events RESTART IDENTITY CASCADE;

INSERT INTO categories(id, name)
SELECT g, (ARRAY['games','tools','garden','audio','kitchen','books','video','outdoor','office','beauty','toys','pets'])[g]
FROM generate_series(1, 12) g;

INSERT INTO customers(n, email, country)
SELECT g, 'customer' || g || '@example.com', (ARRAY['US','FR','DE','JP','BR'])[1 + (g % 5)]
FROM generate_series(1, :customers) g;

INSERT INTO products(id, name, category_id, price_cents, in_stock)
SELECT g,
       'Product ' || g || ' ' ||
         (ARRAY['bravo','crimson','delta','echo','falcon','garnet','hazel','alpha'])[1 + (g % 8)],
       1 + (g % 12),
       100 + (g % 50000),
       (g % 7) <> 0
FROM generate_series(1, :products) g;

INSERT INTO orders(id, customer_id, placed_at, status)
SELECT g, c.id,
       now() - ((g % 365) || ' days')::interval - ((g % 86400) || ' seconds')::interval,
       (ARRAY['paid','paid','paid','refunded','pending'])[1 + (g % 5)]
FROM generate_series(1, :orders) g
JOIN customers c ON c.n = 1 + ((g - 1) % :customers);

-- Exactly two line items per order (composite PK (order_id, line)).
INSERT INTO order_items(order_id, line, product_id, qty, unit_cents)
SELECT 1 + ((g - 1) / 2),
       1 + ((g - 1) % 2),
       1 + (g % :products),
       1 + (g % 5),
       100 + (g % 50000)
FROM generate_series(1, :orders * 2) g;

INSERT INTO reviews(id, product_id, customer_id, rating, body)
SELECT g,
       1 + (g % :products),
       c.id,
       1 + (g % 5),
       (ARRAY['great product','works as expected','would buy again','not impressed',
              'excellent value','broke quickly','highly recommended'])[1 + (g % 7)] || ' #' || g
FROM generate_series(1, :reviews) g
JOIN customers c ON c.n = 1 + ((g - 1) % :customers);

INSERT INTO events(id, product_id, kind, at)
SELECT g, 1 + (g % :products),
       (ARRAY['view','add_to_cart','purchase'])[1 + (g % 3)],
       now() - ((g % 90) || ' days')::interval
FROM generate_series(1, :events) g;

ANALYZE;
