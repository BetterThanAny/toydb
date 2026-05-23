//! Transaction tests for the in-memory engine.
//!
//! toydb uses snapshot isolation: `BEGIN` clones the engine state, and
//! `ROLLBACK` restores it. `COMMIT` simply discards the snapshot.

use toydb::engine::{Engine, MemoryEngine};
use toydb::executor::{Executor, ResultSet};
use toydb::sql::Parser;
use toydb::types::value::Value;

fn run(engine: &mut MemoryEngine, sql: &str) -> ResultSet {
    let stmt = Parser::parse_one(sql).unwrap_or_else(|e| panic!("parse `{sql}`: {e}"));
    Executor::new(engine)
        .execute(&stmt)
        .unwrap_or_else(|e| panic!("exec `{sql}`: {e}"))
}

fn try_run(engine: &mut MemoryEngine, sql: &str) -> Result<ResultSet, String> {
    let stmt = Parser::parse_one(sql).map_err(|e| e.to_string())?;
    Executor::new(engine)
        .execute(&stmt)
        .map_err(|e| e.to_string())
}

fn run_all(engine: &mut MemoryEngine, sql: &str) {
    for stmt in Parser::parse_all(sql).unwrap() {
        Executor::new(engine).execute(&stmt).unwrap();
    }
}

#[test]
fn rollback_undoes_inserts() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "
        CREATE TABLE t (id INT PRIMARY KEY, v INT);
        INSERT INTO t VALUES (1, 10), (2, 20);
    ",
    );
    run(&mut e, "BEGIN");
    assert!(e.in_transaction());
    run(&mut e, "INSERT INTO t VALUES (3, 30)");
    run(&mut e, "INSERT INTO t VALUES (4, 40)");
    run(&mut e, "ROLLBACK");
    assert!(!e.in_transaction());
    let r = run(&mut e, "SELECT id FROM t ORDER BY id");
    match r {
        ResultSet::Select { rows, .. } => {
            assert_eq!(rows.len(), 2);
        }
        _ => panic!(),
    }
}

#[test]
fn commit_keeps_inserts() {
    let mut e = MemoryEngine::new();
    run_all(&mut e, "CREATE TABLE t (id INT PRIMARY KEY)");
    run(&mut e, "BEGIN");
    run(&mut e, "INSERT INTO t VALUES (1), (2)");
    run(&mut e, "COMMIT");
    let r = run(&mut e, "SELECT id FROM t ORDER BY id");
    match r {
        ResultSet::Select { rows, .. } => assert_eq!(rows.len(), 2),
        _ => panic!(),
    }
}

#[test]
fn rollback_undoes_updates_and_deletes() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "
        CREATE TABLE t (id INT PRIMARY KEY, n INT);
        INSERT INTO t VALUES (1, 10), (2, 20), (3, 30);
    ",
    );
    run(&mut e, "BEGIN");
    run(&mut e, "UPDATE t SET n = n + 100");
    run(&mut e, "DELETE FROM t WHERE id = 1");
    run(&mut e, "ROLLBACK");
    let r = run(&mut e, "SELECT id, n FROM t ORDER BY id");
    match r {
        ResultSet::Select { rows, .. } => {
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[0][1], Value::Integer(10));
            assert_eq!(rows[2][1], Value::Integer(30));
        }
        _ => panic!(),
    }
}

#[test]
fn rollback_undoes_ddl() {
    let mut e = MemoryEngine::new();
    run(&mut e, "BEGIN");
    run(&mut e, "CREATE TABLE t (a INT)");
    run(&mut e, "ROLLBACK");
    assert!(e.list_tables().is_empty());
}

#[test]
fn nested_begin_rejected() {
    let mut e = MemoryEngine::new();
    run(&mut e, "BEGIN");
    let r = try_run(&mut e, "BEGIN");
    assert!(r.is_err());
    assert!(r.unwrap_err().contains("nested transactions"));
}

#[test]
fn commit_or_rollback_without_begin_errors() {
    let mut e = MemoryEngine::new();
    assert!(try_run(&mut e, "COMMIT").is_err());
    assert!(try_run(&mut e, "ROLLBACK").is_err());
}

#[test]
fn changes_visible_within_same_transaction() {
    let mut e = MemoryEngine::new();
    run_all(&mut e, "CREATE TABLE t (a INT PRIMARY KEY)");
    run(&mut e, "BEGIN");
    run(&mut e, "INSERT INTO t VALUES (1), (2)");
    let r = run(&mut e, "SELECT * FROM t");
    // Reads within the same transaction see uncommitted writes.
    match r {
        ResultSet::Select { rows, .. } => assert_eq!(rows.len(), 2),
        _ => panic!(),
    }
    run(&mut e, "ROLLBACK");
}

#[test]
fn savepoint_alternative_using_two_phases() {
    let mut e = MemoryEngine::new();
    run_all(&mut e, "CREATE TABLE t (id INT PRIMARY KEY, v INT)");
    run(&mut e, "BEGIN");
    run(&mut e, "INSERT INTO t VALUES (1, 100)");
    run(&mut e, "COMMIT");
    run(&mut e, "BEGIN");
    run(&mut e, "INSERT INTO t VALUES (2, 200)");
    run(&mut e, "ROLLBACK");
    let r = run(&mut e, "SELECT id, v FROM t ORDER BY id");
    match r {
        ResultSet::Select { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Integer(1));
        }
        _ => panic!(),
    }
}
