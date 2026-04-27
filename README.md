# toydb

A from-scratch SQL database engine in Rust — for learning, not production.

```sql
toydb> CREATE TABLE movies (id INTEGER PRIMARY KEY, title TEXT, year INTEGER);
toydb> INSERT INTO movies VALUES (1, 'Sicario', 2015), (2, 'Arrival', 2016);
toydb> SELECT title, year FROM movies WHERE year > 2015 ORDER BY year;
+---------+------+
| title   | year |
+---------+------+
| Arrival | 2016 |
+---------+------+
```

## What it does

- SQL frontend (lexer, recursive-descent parser, AST)
- Type system (NULL / Bool / Integer / Float / String)
- Expression evaluator (arithmetic, comparison, logic, string)
- Pluggable storage engine: in-memory by default, on-disk with B-tree + WAL
- Executor: scan / project / filter / sort / limit / aggregate / join
- MVCC transactions (BEGIN / COMMIT / ROLLBACK), snapshot isolation
- REPL with table-formatted output

## What it does not

- Network protocol (no Postgres wire / MySQL wire)
- Complex query optimization
- Replication, sharding, distributed consensus

## Build

```bash
cargo build --release
cargo test --all
cargo run --release
```

## Layout

```
src/
├── lib.rs            # public API
├── error.rs          # crate-wide Error
├── sql/              # lexer, parser, AST
├── types/            # Value, DataType, Row
├── catalog/          # table / column metadata
├── engine/           # storage engines (memory, disk)
├── executor/         # query plan + execution
├── storage/          # page, pager, B-tree, WAL
└── txn/              # MVCC transaction layer
bin/toydb.rs          # REPL entry
tests/                # end-to-end SQL tests
```

See `PLAN.md` for milestones and `CLAUDE.md` for project conventions.
