-- toydb demo: txn.sql
-- Showcases BEGIN / COMMIT / ROLLBACK with snapshot isolation.

CREATE TABLE accounts (
    id      INTEGER PRIMARY KEY,
    name    TEXT NOT NULL,
    balance INTEGER NOT NULL
);

INSERT INTO accounts VALUES
    (1, 'alice',  100),
    (2, 'bob',    200),
    (3, 'carol',  300);

-- 1. Successful transfer (alice → bob, 50). COMMIT keeps it.
BEGIN;
UPDATE accounts SET balance = balance - 50 WHERE name = 'alice';
UPDATE accounts SET balance = balance + 50 WHERE name = 'bob';
COMMIT;

SELECT name, balance FROM accounts ORDER BY id;

-- 2. Botched transfer (carol → alice, 1000). ROLLBACK undoes it.
BEGIN;
UPDATE accounts SET balance = balance - 1000 WHERE name = 'carol';
UPDATE accounts SET balance = balance + 1000 WHERE name = 'alice';
-- Imagine a check fails here. We discard the writes.
ROLLBACK;

SELECT name, balance FROM accounts ORDER BY id;
