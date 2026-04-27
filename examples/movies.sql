-- toydb demo: movies.sql
-- Showcases CREATE / INSERT / SELECT / WHERE / ORDER BY / LIMIT.

CREATE TABLE movies (
    id      INTEGER PRIMARY KEY,
    title   TEXT NOT NULL,
    year    INTEGER NOT NULL,
    rating  FLOAT
);

INSERT INTO movies VALUES
    (1, 'Sicario',           2015, 7.6),
    (2, 'Arrival',           2016, 7.9),
    (3, 'Blade Runner 2049', 2017, 8.0),
    (4, 'Dune',              2021, 8.1),
    (5, 'Dune: Part Two',    2024, 8.5);

SELECT title, year, rating
  FROM movies
 WHERE year > 2015
 ORDER BY rating DESC
 LIMIT 3;

SELECT 'highest-rated: ' || title AS message
  FROM movies
 WHERE rating = 8.5;

UPDATE movies SET rating = rating + 0.1 WHERE year >= 2020;

SELECT title, rating FROM movies WHERE rating > 8.0 ORDER BY rating DESC;
