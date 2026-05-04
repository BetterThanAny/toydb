# toydb — implementation plan & status

A from-scratch SQL database engine in Rust, built as a teaching project.
This file tracks both the original plan and what was actually delivered.

## Status: complete ✓

| Layer | Status |
|---|---|
| SQL frontend | Lexer + recursive-descent parser, full positional errors |
| AST | DDL (CREATE / DROP / **ALTER** TABLE), DML (INSERT incl. **INSERT ... SELECT**, UPDATE, DELETE), SELECT (WHERE / GROUP BY / HAVING / ORDER BY / LIMIT / OFFSET / **NULLS FIRST/LAST** / DISTINCT / JOIN / **CASE WHEN**), BEGIN / COMMIT / ROLLBACK, **EXPLAIN** |
| Type system | NULL / BOOLEAN / INTEGER / FLOAT / STRING with three-valued logic |
| Expression engine | Arithmetic / comparison / logic / concat / IS NULL / IN / BETWEEN / LIKE, scalar functions (~20 incl. ABS, ROUND, FLOOR/CEIL, SQRT, LENGTH, LOWER/UPPER, TRIM, REVERSE, REPEAT, REPLACE, SUBSTRING, CONCAT, COALESCE, NULLIF, IFF) |
| Aggregates | COUNT / SUM / AVG / MIN / MAX with GROUP BY, HAVING, **DISTINCT** support |
| Joins | INNER / LEFT / RIGHT (nested-loop) |
| Storage engines | `MemoryEngine` (in-memory) and `DiskEngine` (page-based, slotted, free-list) |
| Durability | Write-ahead log + recovery on open; torn-write tolerant |
| Transactions | Snapshot isolation: `BEGIN` clones state, `ROLLBACK` restores, `COMMIT` discards snapshot (memory engine only) |
| REPL | `toydb` CLI with `--db <path>`, `.tables`, `.schema`, `.help`, multi-line input |

## Milestones (delivered)

- **M0 Skeleton** — Cargo workspace, error type, module stubs, README, PLAN, CLAUDE.md, .gitignore.
- **M1 Lexer** — keyword table, integer/float literals incl. scientific, single-quoted strings (with `''` escape), double-quoted identifiers, comments (`--`, `/* */` nestable), positional errors. 24 unit tests.
- **M2 Parser + AST** — recursive descent + Pratt-climbing for expressions, full operator-precedence ladder, every DDL/DML/Tx statement. 50+ tests.
- **M3 Catalog & Value system** — `Value`, `Row`, `Table`, `Column`, `Catalog` with constraints (PK / NOT NULL / UNIQUE / DEFAULT). Coercion rules, three-valued comparison.
- **M4 Expression evaluator** — `eval_with(expr, &Resolver)`. Three-valued AND / OR / NOT, NULL propagation. SQL `LIKE` with `_` and `%`. Built-in functions.
- **M5 Memory engine + executor** — single-table CRUD end-to-end, projection, `SingleTable` resolver.
- **M6 REPL** — `toydb`, table-rendered output, `.tables`/`.help` meta commands, scripted mode.
- **M7 Aggregates / sort / Joins** — `COUNT`/`SUM`/`AVG`/`MIN`/`MAX`, `GROUP BY`, `HAVING`, `INNER`/`LEFT`/`RIGHT JOIN`, `ORDER BY` with alias resolution, `LIMIT`/`OFFSET`. Wide-row resolver for joins.
- **M8 Persistence** — page (8 KiB slotted), pager (file + cache + free list), super page with magic, write-ahead log, replay-on-open, multi-page chains for tables. `DiskEngine`.
- **M9 Transactions** — snapshot isolation via `BEGIN` cloning state for the memory engine. `COMMIT` discards snapshot, `ROLLBACK` restores.
- **M10 Polish** — `EXPLAIN`, `SELECT DISTINCT`, `COUNT(DISTINCT)`, `CASE WHEN ... END`, `INSERT ... SELECT`, `ALTER TABLE ADD COLUMN`, `NULLS FIRST/LAST`, `.schema` meta command, comprehensive test suite, multiple demos.

## Verification commands

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all
cargo run --release -- examples/library.sql
cargo run --release -- examples/movies.sql
cargo run --release -- examples/orders.sql
cargo run --release -- examples/txn.sql
```

## Test inventory

| Suite | Count | What it covers |
|---|---|---|
| unit (in `src/`) | ~225 | Lexer, parser, expr evaluator, aggregate folding, page/pager/wal, memory & disk engines, executor, format |
| `tests/sql_basic.rs` | 16 | end-to-end CRUD, NULL semantics, unique constraint propagation, mutation error paths |
| `tests/sql_comprehensive.rs` | 2 | "kitchen sink" queries spanning many features |
| `tests/sql_persistence.rs` | 7 | open/close/reopen, multi-page tables, drop survival, WAL, failed update atomicity |
| `tests/sql_stress.rs` | 4 | 5 k row inserts, 2 k×2 k join, 50× repeated update, disk round-trip |
| `tests/sql_transaction.rs` | 8 | BEGIN/COMMIT/ROLLBACK semantics, nested-begin rejection, in-tx visibility |

Total: **262 tests**, all passing, zero clippy warnings.

## Project size

- ~9.2 k lines Rust (`src/` incl. tests)
- ~0.7 k lines integration tests (`tests/`)
- ~0.2 k lines SQL examples (`examples/`)
- 4 demo SQL scripts: `movies.sql`, `orders.sql`, `txn.sql`, `library.sql`

## Limitations (intentional)

- No correlated subqueries, CTEs, or window functions. Uncorrelated scalar
  subqueries and `UNION` / `UNION ALL` are supported.
- No real query optimisation: every join is nested-loop, no indexes.
- No replication / sharding / consensus.
- Toydb is single-threaded — concurrent transactions can't interleave.
- `ALTER TABLE` and transactions only work on the memory engine; the disk engine errors on these and is meant to be opened, written, closed, and reopened in straight line.
- `DECIMAL`/`DATE`/`UUID` types are out of scope; everything is one of five primitives.

## Tools used

| Tool | Why |
|---|---|
| `rustc` 1.95 / `cargo` 1.95 | Compiler & build system (already on machine) |
| `clippy` | Lint with `-D warnings` enforced at every milestone |
| `thiserror` 2.0 | Derive `Error` and `Display` for `crate::Error` |
| `rustyline` 15.0 | REPL line editing + history |
| `pretty_assertions` 1.4 (dev) | Better test diffs |

No other crates pulled in; everything else is hand-written.
