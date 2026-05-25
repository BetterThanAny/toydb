# toydb

A from-scratch SQL database engine in Rust, built as a teaching project.
~13 700 lines of Rust, 288 tests, no production dependencies beyond
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

## 30-Second Summary

| Signal | Details |
| --- | --- |
| Positioning | Rust SQL database engine from scratch, useful for showing storage, query execution, WAL, and index/planner fundamentals. |
| Stack | Rust, hand-written lexer/parser, SQL executor, in-memory + page-based disk engine, WAL, runtime `BTreeMap` secondary indexes. |
| Hard parts | SQL three-valued logic, joins/aggregates/subqueries, slotted pages, WAL replay, snapshot-style transactions, `CREATE INDEX` + `IndexScan` planning. |
| Quick start | `cargo run --release -- examples/index_demo.sql` to see `SeqScan` switch to `IndexScan`. |
| Validation | `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all`. |
| Benchmark / result | 288 tests cover parser/executor/storage/WAL/index behavior; index demo shows update/delete maintenance, and index tests cover disk reopen/rebuild behavior. |

## What it does

| Layer | Capabilities |
|---|---|
| SQL frontend | Hand-written lexer + recursive-descent parser, full positional error reporting |
| AST | DDL (CREATE / DROP / ALTER TABLE, CREATE / DROP INDEX), DML (INSERT incl. INSERT...SELECT, UPDATE, DELETE), SELECT (WHERE / GROUP BY / HAVING / ORDER BY / LIMIT / OFFSET / JOIN / DISTINCT / CASE WHEN), BEGIN / COMMIT / ROLLBACK, EXPLAIN |
| Type system | NULL, BOOLEAN, INTEGER, FLOAT, STRING — with SQL three-valued logic |
| Expressions | Arithmetic, comparison, logic, string concat, IS NULL, IN, BETWEEN, LIKE, `CASE WHEN`, scalar functions (`abs`, `lower`, `upper`, `length`, `substring`, `coalesce`, `nullif`, `iff`, `concat`, ...) |
| Aggregates | `COUNT`, `SUM`, `AVG`, `MIN`, `MAX` with `GROUP BY` / `HAVING` / `DISTINCT` (e.g. `COUNT(DISTINCT col)`) |
| Query shaping | `SELECT DISTINCT`, alias-aware `ORDER BY` (`ORDER BY <SELECT alias>`), `LIMIT` / `OFFSET`, `UNION` / `UNION ALL`, scalar subqueries `(SELECT ...)` |
| Joins | `INNER`, `LEFT`, `RIGHT` (nested-loop) |
| Planning + indexes | `SeqScan` by default; single-column equality predicates can use `IndexScan(index=..., key=...)` via `EXPLAIN SELECT` |
| Storage engines | In-memory `MemoryEngine` and disk-backed `DiskEngine` (page-based, slotted pages, free list) |
| Durability | Write-ahead log, recovery on open, torn-write tolerance |
| Transactions | Snapshot isolation: `BEGIN` clones state, `ROLLBACK` restores, `COMMIT` accepts |
| REPL | Interactive `toydb` CLI with table-formatted output, `.tables`, `.help`, multi-line input |

## What it does NOT

- Network protocol (no Postgres / MySQL wire — single process REPL only)
- Correlated subqueries, CTEs, window functions (only **uncorrelated scalar** subqueries supported)
- `ORDER BY` / `LIMIT` on `UNION`'d results (we accept the unions but reject the trailing clauses; wrap with a query that's more capable than toydb if you need it)
- Cost-based optimisation, statistics, or join reordering
- Range, composite, covering, unique, or expression indexes; secondary indexes are single-column equality only
- Distributed anything (no replication, no sharding, no consensus)
- Concurrent transactions (toydb is single-threaded)
- Persisted index pages: disk indexes persist as catalog metadata and are rebuilt from table pages after open/WAL replay
- VACUUM / type-length enforcement (`VARCHAR(N)` is parsed but `N` ignored)
- ALTER TABLE on the disk engine (memory only — would require row-level rewrite of every page)

## Build & run

```bash
cargo build --release
cargo test --all                    # 288 tests across unit + integration
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

### Indexes and EXPLAIN — `examples/index_demo.sql`

```bash
$ cargo run --release -- examples/index_demo.sql
```
Shows `CREATE INDEX idx_users_age ON users(age)`, `EXPLAIN SELECT ... WHERE age = 20`, equality lookup through `IndexScan`, and index maintenance after `UPDATE` / `DELETE`. Dropping the index returns the same query to `SeqScan`.

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
│   ├── index.rs         runtime BTreeMap index store
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
tests/                 end-to-end SQL tests (basic, CLI, comprehensive, index, persistence, stress, transactions)
examples/              sample SQL scripts
```

## Test inventory

| File | What it covers |
|---|---|
| `tests/sql_basic.rs` | end-to-end CRUD, NULL semantics, constraints, idempotent DDL, mutation error paths |
| `tests/sql_comprehensive.rs` | broad feature pipelines and NULL three-valued logic |
| `tests/sql_persistence.rs` | open/close/reopen, WAL replay, multi-page tables, drop-table durability |
| `tests/sql_index.rs` | CREATE/DROP INDEX, IndexScan planning, update/delete maintenance, disk reopen/rebuild |
| `tests/sql_stress.rs` | larger in-memory and disk smoke workloads |
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
  Statement errors inside a memory-engine transaction do not automatically
  roll the transaction back, so clients should issue `ROLLBACK` explicitly
  if they want to abandon earlier successful statements in the same `BEGIN`.
- **Secondary indexes are metadata-persistent, tree-rebuilt.** The
  catalog stores `Index { name, table, column }`. Engines maintain a
  runtime `BTreeMap<Value, BTreeSet<RowId>>`; the disk engine rebuilds
  it from recovered table pages on open. That keeps the pager format
  simple and makes WAL replay authoritative, at the cost of O(n) open
  time per indexed table.
- **Planner rule is deliberately small.** For a single-table SELECT,
  `WHERE indexed_col = <constant>` or an `AND` containing that equality
  chooses `IndexScan`; the executor still evaluates the full WHERE
  predicate after fetching candidate rows, so extra filters remain
  correct.
- **Pager is the only thing that touches disk.** Layers above never
  open files directly, which keeps the I/O surface tiny.
- **WAL is intentionally minimal.** No LSN, no checkpoint, no group
  commit — recovery just replays records onto the catalog and pages.

## Index design and limits

Syntax:

```sql
CREATE INDEX idx_users_age ON users(age);
DROP INDEX idx_users_age;
EXPLAIN SELECT id FROM users WHERE age = 20;
```

Plan shape:

```text
SeqScan `users`
  Filter (WHERE)
  Project (2 items)

IndexScan `users` (index=idx_users_age, key=20)
  Filter (WHERE)
  Project (2 items)
```

Complexity:

| Operation | Without index | With equality index |
|---|---:|---:|
| `WHERE col = v` candidate lookup | O(n) row scan | O(log m + k) in the runtime B-tree |
| `INSERT` | O(1) append/page insert plus constraints | plus O(log m) per index |
| `UPDATE indexed_col` | O(n) to find target today | plus remove/insert O(log m) |
| `DELETE` | O(n) to find target today | plus remove O(log m) |
| Disk open | O(catalog) | O(rows in indexed tables) rebuild |

Small benchmark / smoke workload:

```bash
cargo run --release -- examples/index_demo.sql
```

The demo prints the same `SELECT ... WHERE age = 20` results before and
after index creation. The visible benchmark signal is the plan change:
the first `EXPLAIN` emits `SeqScan`, the second emits
`IndexScan (index=idx_users_age, key=20)`. For larger tables this changes
candidate discovery from scanning every row to probing the B-tree and
then evaluating the remaining WHERE predicate over only matching row ids.

See `PLAN.md` for the milestone-by-milestone walkthrough and
`CLAUDE.md` for the in-repo coding conventions.
