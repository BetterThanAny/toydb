//! End-to-end SQL tests: parse + execute through `MemoryEngine`.
//!
//! These exercise the public surface (Parser + Executor) the same way
//! the REPL does, catching wiring bugs that unit tests don't.

use toydb::engine::{Engine, MemoryEngine};
use toydb::executor::{Executor, ResultSet};
use toydb::sql::Parser;
use toydb::types::value::Value;

fn run(engine: &mut MemoryEngine, sql: &str) -> ResultSet {
    let stmt = Parser::parse_one(sql).unwrap_or_else(|e| panic!("parse `{sql}`: {e}"));
    Executor::new(engine).execute(&stmt).unwrap_or_else(|e| panic!("exec `{sql}`: {e}"))
}

fn run_all(engine: &mut MemoryEngine, sql: &str) {
    for stmt in Parser::parse_all(sql).unwrap() {
        Executor::new(engine).execute(&stmt).unwrap();
    }
}

#[test]
fn basic_pipeline() {
    let mut e = MemoryEngine::new();
    run_all(&mut e, "
        CREATE TABLE users (id INT PRIMARY KEY, name TEXT NOT NULL, age INT);
        INSERT INTO users VALUES (1, 'alice', 30), (2, 'bob', 25), (3, 'carol', 40);
    ");

    let r = run(&mut e, "SELECT name FROM users WHERE age > 28 ORDER BY age");
    match r {
        ResultSet::Select { rows, .. } => {
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0][0], Value::String("alice".into()));
            assert_eq!(rows[1][0], Value::String("carol".into()));
        }
        _ => panic!(),
    }
}

#[test]
fn null_in_arithmetic_is_null() {
    let mut e = MemoryEngine::new();
    run_all(&mut e, "
        CREATE TABLE t (a INT, b INT);
        INSERT INTO t VALUES (1, 1), (2, NULL), (3, 3);
    ");
    let r = run(&mut e, "SELECT a + b FROM t ORDER BY a");
    match r {
        ResultSet::Select { rows, .. } => {
            assert_eq!(rows[0][0], Value::Integer(2));
            assert_eq!(rows[1][0], Value::Null);
            assert_eq!(rows[2][0], Value::Integer(6));
        }
        _ => panic!(),
    }
}

#[test]
fn unique_violation_propagates() {
    let mut e = MemoryEngine::new();
    run_all(&mut e, "
        CREATE TABLE t (id INT PRIMARY KEY, email TEXT UNIQUE);
        INSERT INTO t VALUES (1, 'a@x'), (2, 'b@x');
    ");
    let stmt = Parser::parse_one("INSERT INTO t VALUES (3, 'a@x')").unwrap();
    let r = Executor::new(&mut e).execute(&stmt);
    assert!(r.is_err());
    assert!(r.unwrap_err().to_string().contains("duplicate"));
}

#[test]
fn update_then_delete() {
    let mut e = MemoryEngine::new();
    run_all(&mut e, "
        CREATE TABLE t (id INT PRIMARY KEY, n INT);
        INSERT INTO t VALUES (1, 10), (2, 20), (3, 30);
        UPDATE t SET n = n * 2;
        DELETE FROM t WHERE n > 40;
    ");
    let r = run(&mut e, "SELECT id, n FROM t ORDER BY id");
    match r {
        ResultSet::Select { rows, .. } => {
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0][1], Value::Integer(20));
            assert_eq!(rows[1][1], Value::Integer(40));
        }
        _ => panic!(),
    }
}

#[test]
fn case_insensitive_keywords_case_sensitive_ids() {
    let mut e = MemoryEngine::new();
    run_all(&mut e, "create TABLE Users (Id int primary key, Name text);");
    let r = run(&mut e, "INSERT INTO Users VALUES (1, 'alice')");
    match r {
        ResultSet::Insert { count } => assert_eq!(count, 1),
        _ => panic!(),
    }
    // lowercase `users` should error
    let stmt = Parser::parse_one("SELECT * FROM users").unwrap();
    assert!(Executor::new(&mut e).execute(&stmt).is_err());
}

#[test]
fn multi_value_insert() {
    let mut e = MemoryEngine::new();
    run_all(&mut e, "CREATE TABLE t (id INT PRIMARY KEY)");
    let r = run(&mut e, "INSERT INTO t VALUES (1), (2), (3), (4), (5)");
    match r {
        ResultSet::Insert { count } => assert_eq!(count, 5),
        _ => panic!(),
    }
}

#[test]
fn create_if_not_exists_is_idempotent() {
    let mut e = MemoryEngine::new();
    run_all(&mut e, "CREATE TABLE t (a INT)");
    run_all(&mut e, "CREATE TABLE IF NOT EXISTS t (a INT)");
    run_all(&mut e, "CREATE TABLE IF NOT EXISTS t (a INT)");
    assert_eq!(e.list_tables().len(), 1);
}
