# toydb

A from-scratch SQL database engine in Rust, built as a teaching project.
~5 000 lines of code, ~210 tests, no production dependencies beyond
`thiserror` and `rustyline`.

```sql
toydb> CREATE TABLE movies (id INT PRIMARY KEY, title TEXT, year INT, rating FLOAT);
toydb> INSERT INTO movies VALUES (1,'Sicario',2015,7.6),(2,'Arrival',2016,7.9),(3,'Dune',2021,8.1);
toydb> SELECT title, year FROM movies WHERE year >= 2016 ORDER BY rating DESC LIMIT 2;
+---------+------+
| title   | year |
+---------+------+
| Dune    | 2021 |
| Arrival | 2016 |
+---------+------+
(2 rows)
```

## What it does

| Layer | Capabilities |
|---|---|
| SQL frontend | Hand-written lexer + recursive-descent parser, full positional error reporting |
| AST | DDL (CREATE / DROP / ALTER TABLE), DML (INSERT incl. INSERT...SELECT, UPDATE, DELETE), SELECT (WHERE / GROUP BY / HAVING / ORDER BY / LIMIT / OFFSET / JOIN / DISTINCT / CASE WHEN), BEGIN / COMMIT / ROLLBACK, EXPLAIN |
| Type system | NULL, BOOLEAN, INTEGER, FLOAT, STRING — with SQL three-valued logic |
| Expressions | Arithmetic, comparison, logic, string concat, IS NULL, IN, BETWEEN, LIKE, `CASE WHEN`, scalar functions (`abs`, `lower`, `upper`, `length`, `substring`, `coalesce`, `nullif`, `iff`, `concat`, ...) |
| Aggregates | `COUNT`, `SUM`, `AVG`, `MIN`, `MAX` with `GROUP BY` / `HAVING` / `DISTINCT` (e.g. `COUNT(DISTINCT col)`) |
| Query shaping | `SELECT DISTINCT`, alias-aware `ORDER BY` (`ORDER BY <SELECT alias>`), `LIMIT` / `OFFSET`, `UNION` / `UNION ALL`, scalar subqueries `(SELECT ...)` |
| Joins | `INNER`, `LEFT`, `RIGHT` (nested-loop) |
| Storage engines | In-memory `MemoryEngine` and disk-backed `DiskEngine` (page-based, slotted pages, free list) |
| Durability | Write-ahead log, recovery on open, torn-write tolerance |
| Transactions | Snapshot isolation: `BEGIN` clones state, `ROLLBACK` restores, `COMMIT` accepts |
| REPL | Interactive `toydb` CLI with table-formatted output, `.tables`, `.help`, multi-line input |

## What it does NOT

- Network protocol (no Postgres / MySQL wire — single process REPL only)
- Correlated subqueries, CTEs, window functions (only **uncorrelated scalar** subqueries supported)
- `ORDER BY` / `LIMIT` on `UNION`'d results (we accept the unions but reject the trailing clauses; wrap with a query that's more capable than toydb if you need it)
- Real query optimisation (no indexes, no statistics, plan = nested loop everywhere)
- Distributed anything (no replication, no sharding, no consensus)
- Concurrent transactions (toydb is single-threaded)
- B-tree / hash indexes (storage is unindexed, scans are O(n))
- VACUUM / type-length enforcement (`VARCHAR(N)` is parsed but `N` ignored)
- ALTER TABLE on the disk engine (memory only — would require row-level rewrite of every page)

## Build & run

```bash
cargo build --release
cargo test --all                    # ~210 tests across unit + integration
cargo clippy --all-targets -- -D warnings

# REPL — in memory
cargo run --release

# REPL — durable
cargo run --release -- --db ./mydb.toydb

# Run a SQL script
cargo run --release -- examples/movies.sql
cargo run --release -- --db ./mydb.toydb examples/orders.sql
```

## Tour by example

### Basics — `examples/movies.sql`

```bash
$ cargo run --release -- examples/movies.sql
```
Demonstrates `CREATE TABLE`, multi-row `INSERT`, `WHERE` / `ORDER BY` / `LIMIT`, expression aliases, `UPDATE`.

### Joins, group-by, aggregates — `examples/orders.sql`

```bash
$ cargo run --release -- examples/orders.sql
```
Builds two tables, runs `INNER JOIN` and `LEFT JOIN`, groups by customer with `COUNT(*)`, `SUM`, `AVG`, applies `HAVING`, refers to SELECT aliases in `ORDER BY`.

### Transactions — `examples/txn.sql`

```bash
$ cargo run --release -- examples/txn.sql
```
Shows a successful transfer (`BEGIN ... COMMIT`) and an aborted one (`BEGIN ... ROLLBACK`), proving the rollback fully restored balance state.

### Everything together — `examples/library.sql`

```bash
$ cargo run --release -- examples/library.sql
```
Mixes joins, group-by, having, `LIKE` / `IN` / `BETWEEN`, `CASE WHEN ... END`, computed columns, transactions — a small SF/F bibliography.

### Persistence

```bash
$ rm -f /tmp/mydb.toydb /tmp/mydb.toydb-wal
$ cargo run --release -- --db /tmp/mydb.toydb <<EOF
CREATE TABLE k (id INT PRIMARY KEY, v TEXT);
INSERT INTO k VALUES (1,'one'),(2,'two'),(3,'three');
EOF
$ cargo run --release -- --db /tmp/mydb.toydb <<EOF
SELECT * FROM k ORDER BY id;
EOF
```

The second invocation reads the rows the first one inserted.

### REPL meta commands

```text
toydb> .tables
toydb> .schema users
toydb> .help
toydb> .exit
```

## Layout

```
src/
├── lib.rs             public API
├── error.rs           crate-wide Error
├── format.rs          ASCII-grid result rendering
├── sql/               lexer, parser, AST
├── types/             Value, DataType, Row primitives
├── catalog/           Table / Column metadata
├── engine/            storage backends
│   ├── memory.rs        in-memory + transactions via clone-on-begin
│   └── disk.rs          disk-backed via pager + WAL
├── executor/          query planner + execution
│   ├── plan.rs          statement dispatcher, FROM/WHERE/SELECT
│   ├── expr.rs          scalar expression evaluator
│   └── aggregate.rs     COUNT / SUM / AVG / MIN / MAX, group folding
└── storage/           low-level on-disk primitives
    ├── encoding.rs      hand-rolled binary serialiser
    ├── page.rs          fixed 8 KiB slotted page
    ├── pager.rs         file-backed paged storage with cache
    └── wal.rs           append-only write-ahead log
bin/toydb.rs           REPL entry
tests/                 end-to-end SQL tests (basic, persistence, transactions)
examples/              sample SQL scripts
```

## Test inventory

| File | What it covers |
|---|---|
| `tests/sql_basic.rs` | end-to-end CRUD, NULL semantics, unique constraints, idempotent DDL |
| `tests/sql_persistence.rs` | open/close/reopen, WAL replay, multi-page tables, drop-table durability |
| `tests/sql_transaction.rs` | BEGIN/COMMIT/ROLLBACK, snapshot semantics, nested-begin rejection |
| each `src/<mod>/*.rs` | tight per-module unit tests (lexer, parser, expr, aggregate, page, pager, wal, ...) |

## Design notes

- **AST carries strings, not source slices.** Frees the planner from
  source-tied lifetimes and makes errors stand-alone.
- **Expression evaluation is uniform.** WHERE, ORDER BY, projection, and
  HAVING all walk the same `eval_with` path; only the `Resolver`
  changes (single-table, wide for joins, group-aware for aggregates).
- **Aggregates dedupe by structural AST equality.** `SUM(price)` reused
  in SELECT and HAVING shares one accumulator per group.
- **Snapshot isolation = clone-on-begin.** Cheap and obvious for a
  teaching engine; the disk engine does not yet support transactions.
- **Pager is the only thing that touches disk.** Layers above never
  open files directly, which keeps the I/O surface tiny.
- **WAL is intentionally minimal.** No LSN, no checkpoint, no group
  commit — recovery just replays records onto the catalog and pages.

See `PLAN.md` for the milestone-by-milestone walkthrough and
`CLAUDE.md` for the in-repo coding conventions.
