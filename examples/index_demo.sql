-- Single-column index demo.
-- Run with:
--   cargo run --release -- examples/index_demo.sql

CREATE TABLE users (id INT PRIMARY KEY, name TEXT NOT NULL, age INT, city TEXT);

INSERT INTO users VALUES
    (1, 'alice', 20, 'Shanghai'),
    (2, 'bob', 30, 'Beijing'),
    (3, 'carol', 20, 'Shenzhen'),
    (4, 'dave', 40, 'Hangzhou'),
    (5, 'erin', 20, 'Shanghai');

EXPLAIN SELECT id, name FROM users WHERE age = 20 ORDER BY id;
SELECT id, name FROM users WHERE age = 20 ORDER BY id;

CREATE INDEX idx_users_age ON users(age);

EXPLAIN SELECT id, name FROM users WHERE age = 20 ORDER BY id;
SELECT id, name FROM users WHERE age = 20 ORDER BY id;

UPDATE users SET age = 20 WHERE id = 4;
DELETE FROM users WHERE id = 1;

EXPLAIN SELECT id, name FROM users WHERE age = 20 AND city = 'Shanghai' ORDER BY id;
SELECT id, name FROM users WHERE age = 20 AND city = 'Shanghai' ORDER BY id;

DROP INDEX idx_users_age;
EXPLAIN SELECT id, name FROM users WHERE age = 20 ORDER BY id;
