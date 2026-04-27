-- toydb demo: library.sql
-- A small library catalog showing every major feature in one script.

CREATE TABLE authors (
    id    INTEGER PRIMARY KEY,
    name  TEXT NOT NULL UNIQUE,
    born  INTEGER
);

CREATE TABLE books (
    id        INTEGER PRIMARY KEY,
    author_id INTEGER NOT NULL,
    title     TEXT NOT NULL,
    year      INTEGER NOT NULL,
    pages     INTEGER,
    in_stock  BOOLEAN DEFAULT TRUE
);

INSERT INTO authors VALUES
    (1, 'Ursula K. Le Guin',     1929),
    (2, 'N. K. Jemisin',         1972),
    (3, 'Frank Herbert',         1920),
    (4, 'Octavia E. Butler',     1947);

INSERT INTO books (id, author_id, title, year, pages) VALUES
    (1, 1, 'A Wizard of Earthsea',     1968, 205),
    (2, 1, 'The Left Hand of Darkness', 1969, 304),
    (3, 1, 'The Dispossessed',          1974, 387),
    (4, 2, 'The Fifth Season',          2015, 468),
    (5, 2, 'The Obelisk Gate',          2016, 410),
    (6, 2, 'The Stone Sky',             2017, 450),
    (7, 3, 'Dune',                       1965, 412),
    (8, 3, 'Dune Messiah',               1969, 256),
    (9, 4, 'Kindred',                    1979, 287);

-- Mark some out of stock.
UPDATE books SET in_stock = FALSE WHERE id IN (2, 8);

-- 1. JOIN: every book with author name.
SELECT a.name AS author, b.title, b.year
  FROM authors a INNER JOIN books b ON a.id = b.author_id
 ORDER BY b.year;

-- 2. GROUP BY: how many books per author?
SELECT a.name AS author, COUNT(*) AS book_count, MIN(b.year) AS first_year
  FROM authors a INNER JOIN books b ON a.id = b.author_id
 GROUP BY a.name
 ORDER BY book_count DESC, author;

-- 3. HAVING: authors with average page count >= 350.
SELECT a.name, AVG(b.pages) AS avg_pages
  FROM authors a INNER JOIN books b ON a.id = b.author_id
 GROUP BY a.name
 HAVING AVG(b.pages) >= 350
 ORDER BY avg_pages DESC;

-- 4. LEFT JOIN: include authors with no qualifying books.
SELECT a.name, COUNT(*) AS post_2000
  FROM authors a LEFT JOIN books b ON a.id = b.author_id AND b.year >= 2000
 GROUP BY a.name
 ORDER BY post_2000 DESC, a.name;

-- 5. Expressions: pages-per-decade-since-debut.
SELECT a.name,
       MIN(b.year) AS debut,
       SUM(b.pages) AS total_pages,
       SUM(b.pages) / (2025 - MIN(b.year)) AS pages_per_year
  FROM authors a INNER JOIN books b ON a.id = b.author_id
 GROUP BY a.name
 ORDER BY pages_per_year DESC;

-- 6. WHERE with LIKE / IN / BETWEEN.
SELECT title, year
  FROM books
 WHERE title LIKE 'The %'
   AND year BETWEEN 1960 AND 2020
   AND author_id IN (1, 2)
 ORDER BY year;

-- 7. CASE expression for categorisation.
SELECT title,
       year,
       CASE WHEN year < 1980 THEN 'classic'
            WHEN year < 2000 THEN 'modern'
            ELSE 'contemporary'
       END AS era
  FROM books
 ORDER BY year;

-- 8. Transactional batch update.
BEGIN;
UPDATE books SET in_stock = TRUE WHERE id = 2;
DELETE FROM books WHERE pages IS NULL;
SELECT COUNT(*) AS in_stock_books FROM books WHERE in_stock = TRUE;
COMMIT;
