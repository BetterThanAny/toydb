-- toydb demo: orders.sql
-- Showcases JOIN, GROUP BY, HAVING, aggregates.

CREATE TABLE customers (
    id    INTEGER PRIMARY KEY,
    name  TEXT NOT NULL,
    city  TEXT
);

CREATE TABLE orders (
    id          INTEGER PRIMARY KEY,
    customer_id INTEGER NOT NULL,
    amount      FLOAT,
    paid        BOOLEAN
);

INSERT INTO customers VALUES
    (1, 'alice',   'Beijing'),
    (2, 'bob',     'Shanghai'),
    (3, 'carol',   'Beijing'),
    (4, 'dave',    'Shanghai');

INSERT INTO orders VALUES
    (1, 1,  99.0, TRUE),
    (2, 1, 150.0, TRUE),
    (3, 2, 200.0, FALSE),
    (4, 2,  75.5, TRUE),
    (5, 3, 300.0, TRUE),
    (6, 1,  NULL, TRUE);

-- INNER JOIN: customers with their orders
SELECT c.name, o.id AS order_id, o.amount
  FROM customers c INNER JOIN orders o ON c.id = o.customer_id
 ORDER BY c.name, o.id;

-- LEFT JOIN: every customer (Dave has no orders, Carol has one)
SELECT c.name, COUNT(*) AS order_count, SUM(o.amount) AS total
  FROM customers c LEFT JOIN orders o ON c.id = o.customer_id
 GROUP BY c.name
 ORDER BY total DESC;

-- HAVING: only customers with >= 2 paid orders
SELECT c.name, COUNT(*) AS paid_orders
  FROM customers c INNER JOIN orders o ON c.id = o.customer_id
 WHERE o.paid = TRUE
 GROUP BY c.name
 HAVING COUNT(*) >= 2
 ORDER BY paid_orders DESC;

-- City-level aggregate
SELECT city, COUNT(*) AS customer_count, AVG(o.amount) AS avg_order
  FROM customers c LEFT JOIN orders o ON c.id = o.customer_id
 GROUP BY city
 ORDER BY city;
