# Changelog

This file tracks the milestone-to-milestone evolution of toydb.

## Unreleased / current

- **M10** — `EXPLAIN`, `SELECT DISTINCT`, `COUNT(DISTINCT col)`, `CASE WHEN ... END`,
  `INSERT ... SELECT`, `ALTER TABLE ADD COLUMN`, `NULLS FIRST/LAST`, REPL `.schema`
  meta command, additional built-in functions (`REVERSE`, `REPEAT`, `REPLACE`),
  comprehensive integration tests, polished `README` / `PLAN.md`.

## M9 — Transactions

- `MemoryEngine::begin/commit/rollback` via clone-on-begin, snapshot restore.
- `BEGIN`, `COMMIT`, `ROLLBACK` statements wired through executor.
- 8 transaction tests in `tests/sql_transaction.rs`.

## M8 — Persistence

- `storage::encoding` — hand-rolled binary serialiser for `Value` / `Row` / `Table`.
- `storage::page` — 8 KiB slotted page (insert/update/delete with tombstones).
- `storage::pager` — file-backed paged storage, super page with magic + free list,
  small write-back cache, fsync on flush.
- `storage::wal` — append-only WAL with type-tagged records and torn-tail tolerance.
- `engine::disk` — `DiskEngine` implements `Engine` over the pager + WAL chain;
  catalog persists in linked catalog pages; WAL replays on open.
- 5 disk-roundtrip tests in `tests/sql_persistence.rs`.

## M7 — Aggregates / sort / Joins

- `executor::aggregate` — `COUNT`, `SUM`, `AVG`, `MIN`, `MAX` with `Accumulator`,
  group-key map, fold-row API, per-group projection via `eval_in_group`.
- `WideSchema` / `WideResolver` for column lookup across joined sources.
- Nested-loop join (`INNER` / `LEFT` / `RIGHT`) materialised as wide rows.
- `ORDER BY <select alias>` resolution via expression rewrite.

## M6 — REPL

- `toydb` binary using `rustyline`. `--db <path>` flag toggles disk engine.
- ASCII-grid result rendering in `format::render`.
- `.tables`, `.help` meta commands; multi-line input until trailing `;`.

## M5 — Memory engine + Executor

- `MemoryEngine` with primary key / unique / not-null enforcement.
- `Executor` orchestrating dispatch over `Statement` variants.
- Single-table SELECT with WHERE, projection, ORDER BY, LIMIT.
- Constant SELECT (`SELECT 1+1`) without FROM.

## M4 — Expression evaluator

- `eval_with(&Expression, &dyn Resolver)` — pure scalar evaluation.
- Three-valued logic for AND / OR / NOT including NULL.
- IS NULL / IN / BETWEEN / LIKE.
- ~15 built-in scalar functions.

## M3 — Catalog & Value system

- `Value` enum (NULL / Boolean / Integer / Float / String) with coercion,
  three-valued comparison, total ordering for index keys.
- `Row` as positional `Vec<Value>`.
- `Table` and `Column` schema with constraint flags.
- `Catalog` indexed by table name.

## M2 — SQL parser + AST

- Recursive-descent parser with Pratt expression climbing.
- AST covering all DDL / DML / transaction statements.
- Operator precedence ladder: OR < AND < NOT < cmp < `||` < +/- < */% < ^ < unary < atom.

## M1 — SQL lexer

- Hand-written byte scanner with positional spans, all SQL keywords,
  string / number / identifier literals, line + nestable block comments,
  `<>` and `!=` aliases for not-equal.

## M0 — Skeleton

- `cargo init`, module tree (sql / types / catalog / engine / executor / storage / txn),
  crate-wide `Error` / `Result`, README, PLAN, CLAUDE.md, .gitignore.
