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
fn basic_pipeline() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "
        CREATE TABLE users (id INT PRIMARY KEY, name TEXT NOT NULL, age INT);
        INSERT INTO users VALUES (1, 'alice', 30), (2, 'bob', 25), (3, 'carol', 40);
    ",
    );

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
    run_all(
        &mut e,
        "
        CREATE TABLE t (a INT, b INT);
        INSERT INTO t VALUES (1, 1), (2, NULL), (3, 3);
    ",
    );
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
    run_all(
        &mut e,
        "
        CREATE TABLE t (id INT PRIMARY KEY, email TEXT UNIQUE);
        INSERT INTO t VALUES (1, 'a@x'), (2, 'b@x');
    ",
    );
    let stmt = Parser::parse_one("INSERT INTO t VALUES (3, 'a@x')").unwrap();
    let r = Executor::new(&mut e).execute(&stmt);
    assert!(r.is_err());
    assert!(r.unwrap_err().to_string().contains("duplicate"));
}

#[test]
fn multi_value_insert_is_atomic_on_constraint_failure() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "
        CREATE TABLE u (id INT PRIMARY KEY, v TEXT UNIQUE);
        INSERT INTO u VALUES (1, 'a'), (2, 'b'), (3, 'c');
    ",
    );

    let err = try_run(&mut e, "INSERT INTO u VALUES (4, 'd'), (5, 'a')").unwrap_err();
    assert!(err.contains("duplicate"));

    let r = run(&mut e, "SELECT id, v FROM u ORDER BY id");
    match r {
        ResultSet::Select { rows, .. } => {
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[2][0], Value::Integer(3));
        }
        _ => panic!(),
    }
}

#[test]
fn update_is_atomic_on_constraint_failure() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "
        CREATE TABLE u (id INT PRIMARY KEY, v TEXT UNIQUE);
        INSERT INTO u VALUES (1, 'a'), (2, 'b'), (3, 'c');
    ",
    );

    let err = try_run(&mut e, "UPDATE u SET v = 'x'").unwrap_err();
    assert!(err.contains("duplicate"));

    let r = run(&mut e, "SELECT id, v FROM u ORDER BY id");
    match r {
        ResultSet::Select { rows, .. } => {
            assert_eq!(rows[0][1], Value::String("a".into()));
            assert_eq!(rows[1][1], Value::String("b".into()));
            assert_eq!(rows[2][1], Value::String("c".into()));
        }
        _ => panic!(),
    }
}

#[test]
fn update_can_swap_unique_values() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "
        CREATE TABLE u (id INT PRIMARY KEY, v INT UNIQUE);
        INSERT INTO u VALUES (1, 100), (2, 200);
    ",
    );

    run_all(
        &mut e,
        "UPDATE u SET v = CASE WHEN v = 100 THEN 200 ELSE 100 END",
    );

    let r = run(&mut e, "SELECT id, v FROM u ORDER BY id");
    match r {
        ResultSet::Select { rows, .. } => {
            assert_eq!(rows[0][1], Value::Integer(200));
            assert_eq!(rows[1][1], Value::Integer(100));
        }
        _ => panic!(),
    }
}

#[test]
fn duplicate_insert_columns_are_rejected() {
    let mut e = MemoryEngine::new();
    run_all(&mut e, "CREATE TABLE t (a INT, b INT)");

    let err = try_run(&mut e, "INSERT INTO t (a, a) VALUES (1, 2)").unwrap_err();
    assert!(err.contains("specified more than once"));

    let r = run(&mut e, "SELECT COUNT(*) FROM t");
    match r {
        ResultSet::Select { rows, .. } => {
            assert_eq!(rows[0][0], Value::Integer(0));
        }
        _ => panic!(),
    }
}

#[test]
fn duplicate_update_columns_are_rejected() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "
        CREATE TABLE t (id INT PRIMARY KEY, n INT);
        INSERT INTO t VALUES (1, 10);
    ",
    );

    let err = try_run(&mut e, "UPDATE t SET n = 20, n = 30 WHERE id = 1").unwrap_err();
    assert!(err.contains("specified more than once"));

    let r = run(&mut e, "SELECT n FROM t");
    match r {
        ResultSet::Select { rows, .. } => {
            assert_eq!(rows[0][0], Value::Integer(10));
        }
        _ => panic!(),
    }
}

#[test]
fn alter_table_rejects_duplicate_unique_default() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "
        CREATE TABLE t (name TEXT);
        INSERT INTO t VALUES ('a'), ('b');
    ",
    );

    let err = try_run(
        &mut e,
        "ALTER TABLE t ADD COLUMN email TEXT UNIQUE DEFAULT 'same'",
    )
    .unwrap_err();
    assert!(err.contains("duplicate"));

    let r = run(&mut e, "SELECT name FROM t ORDER BY name");
    match r {
        ResultSet::Select { rows, columns } => {
            assert_eq!(columns.len(), 1);
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0][0], Value::String("a".into()));
        }
        _ => panic!(),
    }
    assert!(try_run(&mut e, "SELECT email FROM t").is_err());
}

#[test]
fn alter_table_rejects_unique_default_on_single_existing_row() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "
        CREATE TABLE t (name TEXT);
        INSERT INTO t VALUES ('a');
    ",
    );

    let err = try_run(
        &mut e,
        "ALTER TABLE t ADD COLUMN email TEXT UNIQUE DEFAULT 'same'",
    )
    .unwrap_err();
    assert!(err.contains("duplicate"));
    assert!(try_run(&mut e, "SELECT email FROM t").is_err());
}

#[test]
fn alter_table_rejects_duplicate_primary_key_default() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "
        CREATE TABLE t (name TEXT);
        INSERT INTO t VALUES ('a'), ('b');
    ",
    );

    let err = try_run(
        &mut e,
        "ALTER TABLE t ADD COLUMN id INT PRIMARY KEY DEFAULT 1",
    )
    .unwrap_err();
    assert!(err.contains("duplicate"));

    let r = run(&mut e, "SELECT name FROM t ORDER BY name");
    match r {
        ResultSet::Select { rows, columns } => {
            assert_eq!(columns.len(), 1);
            assert_eq!(rows.len(), 2);
        }
        _ => panic!(),
    }
    assert!(try_run(&mut e, "SELECT id FROM t").is_err());
}

#[test]
fn constant_select_rejects_offset_without_from() {
    let mut e = MemoryEngine::new();
    let err = try_run(&mut e, "SELECT 1 OFFSET 1").unwrap_err();
    assert!(err.contains("OFFSET"));
}

#[test]
fn alter_table_coerces_default_to_column_type() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "
        CREATE TABLE t (name TEXT);
        INSERT INTO t VALUES ('a');
        ALTER TABLE t ADD COLUMN c INT DEFAULT '1';
    ",
    );

    let r = run(&mut e, "SELECT c + 1 FROM t");
    match r {
        ResultSet::Select { rows, .. } => {
            assert_eq!(rows[0][0], Value::Integer(2));
        }
        _ => panic!(),
    }
}

#[test]
fn alter_table_rejects_bad_default_type_without_schema_change() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "
        CREATE TABLE t (name TEXT);
        INSERT INTO t VALUES ('a');
    ",
    );

    let err = try_run(&mut e, "ALTER TABLE t ADD COLUMN c INT DEFAULT 'x'").unwrap_err();
    assert!(err.contains("cannot parse"));
    assert!(try_run(&mut e, "SELECT c FROM t").is_err());
}

#[test]
fn create_table_rejects_bad_default_type() {
    let mut e = MemoryEngine::new();
    let err = try_run(
        &mut e,
        "CREATE TABLE t (id INT PRIMARY KEY, n INT DEFAULT 'bad')",
    )
    .unwrap_err();
    assert!(err.contains("DEFAULT"), "{err}");
    assert!(try_run(&mut e, "SELECT * FROM t").is_err());
}

#[test]
fn primary_key_null_is_still_not_nullable() {
    let mut e = MemoryEngine::new();
    run_all(&mut e, "CREATE TABLE t (id INT PRIMARY KEY NULL)");

    let err = try_run(&mut e, "INSERT INTO t VALUES (NULL)").unwrap_err();
    assert!(err.contains("NOT NULL"));
}

#[test]
fn alter_table_rejects_primary_key_null_default() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "
        CREATE TABLE t (name TEXT);
        INSERT INTO t VALUES ('a');
    ",
    );

    let err = try_run(
        &mut e,
        "ALTER TABLE t ADD COLUMN id INT PRIMARY KEY NULL DEFAULT NULL",
    )
    .unwrap_err();
    assert!(err.contains("NOT NULL"));
    assert!(try_run(&mut e, "SELECT id FROM t").is_err());
}

#[test]
fn update_then_delete() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "
        CREATE TABLE t (id INT PRIMARY KEY, n INT);
        INSERT INTO t VALUES (1, 10), (2, 20), (3, 30);
        UPDATE t SET n = n * 2;
        DELETE FROM t WHERE n > 40;
    ",
    );
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
    run_all(
        &mut e,
        "create TABLE Users (Id int primary key, Name text);",
    );
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
fn mixed_union_and_union_all_is_left_associative() {
    let mut e = MemoryEngine::new();
    let r = run(&mut e, "SELECT 1 AS x UNION SELECT 1 UNION ALL SELECT 1");
    match r {
        ResultSet::Select { rows, .. } => {
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0][0], Value::Integer(1));
            assert_eq!(rows[1][0], Value::Integer(1));
        }
        _ => panic!(),
    }
}

#[test]
fn union_distinguishes_large_int_from_rounded_float() {
    let mut e = MemoryEngine::new();
    let r = run(
        &mut e,
        "SELECT 9007199254740993 UNION SELECT 9007199254740992.0",
    );
    match r {
        ResultSet::Select { rows, .. } => {
            assert_eq!(rows.len(), 2);
            assert!(
                rows.iter()
                    .any(|row| row[0] == Value::Float(9_007_199_254_740_992.0))
            );
            assert!(
                rows.iter()
                    .any(|row| row[0] == Value::Integer(9_007_199_254_740_993))
            );
        }
        _ => panic!(),
    }
}

#[test]
fn explain_rejects_unsupported_union_order_by() {
    let mut e = MemoryEngine::new();
    let err = try_run(&mut e, "EXPLAIN SELECT 1 UNION SELECT 2 ORDER BY 1").unwrap_err();
    assert!(err.contains("not supported"), "{err}");
}

#[test]
fn group_by_can_order_by_aggregate_not_in_select() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "
        CREATE TABLE t (a INT, b INT);
        INSERT INTO t VALUES (1, 10), (1, 20), (2, 5);
    ",
    );
    let r = run(
        &mut e,
        "SELECT a FROM t GROUP BY a ORDER BY COUNT(*) DESC, a",
    );
    match r {
        ResultSet::Select { rows, .. } => {
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0][0], Value::Integer(1));
            assert_eq!(rows[1][0], Value::Integer(2));
        }
        _ => panic!(),
    }
}

#[test]
fn group_by_can_having_on_select_alias() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "
        CREATE TABLE t (g INT, v INT);
        INSERT INTO t VALUES (1, 10), (1, 20), (2, 5);
    ",
    );
    let r = run(
        &mut e,
        "SELECT g, SUM(v) AS s FROM t GROUP BY g HAVING s > 15 ORDER BY g",
    );
    match r {
        ResultSet::Select { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Integer(1));
            assert_eq!(rows[0][1], Value::Integer(30));
        }
        _ => panic!(),
    }
}

#[test]
fn grouped_having_short_circuits_boolean_ops() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "
        CREATE TABLE t (id INT);
        INSERT INTO t VALUES (1);
    ",
    );
    let r = run(
        &mut e,
        "SELECT COUNT(*) FROM t HAVING COUNT(*) = 1 OR 1 / 0 = 1",
    );
    match r {
        ResultSet::Select { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Integer(1));
        }
        _ => panic!(),
    }
}

#[test]
fn having_without_group_or_aggregate_is_rejected() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "
        CREATE TABLE t (id INT);
        INSERT INTO t VALUES (1), (2);
    ",
    );
    let err = try_run(&mut e, "SELECT id FROM t HAVING id > 1").unwrap_err();
    assert!(err.contains("GROUP BY"), "{err}");
}

#[test]
fn aggregate_const_select_uses_implicit_row() {
    let mut e = MemoryEngine::new();
    let r = run(
        &mut e,
        "SELECT COUNT(*), COUNT(NULL), SUM(1), AVG(1), MIN(1), MAX(1)",
    );
    match r {
        ResultSet::Select { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Integer(1));
            assert_eq!(rows[0][1], Value::Integer(0));
            assert_eq!(rows[0][2], Value::Integer(1));
            assert_eq!(rows[0][3], Value::Float(1.0));
            assert_eq!(rows[0][4], Value::Integer(1));
            assert_eq!(rows[0][5], Value::Integer(1));
        }
        _ => panic!(),
    }
}

#[test]
fn no_from_having_filters_implicit_group() {
    let mut e = MemoryEngine::new();
    let r = run(&mut e, "SELECT COUNT(*) HAVING COUNT(*) = 0");
    match r {
        ResultSet::Select { rows, .. } => assert!(rows.is_empty()),
        _ => panic!(),
    }
}

#[test]
fn correlated_scalar_subquery_is_rejected_explicitly() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "
        CREATE TABLE t (id INT, n INT);
        INSERT INTO t VALUES (1, 10), (2, 20);
    ",
    );
    let err = try_run(
        &mut e,
        "SELECT id FROM t outer_t WHERE n = (SELECT n FROM t WHERE id = outer_t.id)",
    )
    .unwrap_err();
    assert!(err.contains("correlated subqueries"), "{err}");
}

#[test]
fn self_join_without_unique_alias_is_rejected() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "
        CREATE TABLE t (id INT);
        INSERT INTO t VALUES (1), (2);
    ",
    );
    let err = try_run(&mut e, "SELECT * FROM t JOIN t ON t.id = t.id").unwrap_err();
    assert!(err.contains("duplicate table alias"), "{err}");
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
fn distinct_is_applied_before_offset() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "
        CREATE TABLE t (id INT PRIMARY KEY, c INT);
        INSERT INTO t VALUES (1, 10), (2, 10), (3, 20);
    ",
    );
    let r = run(&mut e, "SELECT DISTINCT c FROM t ORDER BY c OFFSET 1");
    match r {
        ResultSet::Select { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Integer(20));
        }
        _ => panic!(),
    }
}

#[test]
fn limit_null_is_unbounded() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "
        CREATE TABLE t (id INT PRIMARY KEY);
        INSERT INTO t VALUES (1), (2), (3);
    ",
    );
    let r = run(&mut e, "SELECT id FROM t ORDER BY id LIMIT NULL");
    match r {
        ResultSet::Select { rows, .. } => {
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[2][0], Value::Integer(3));
        }
        _ => panic!(),
    }
}

#[test]
fn count_distinct_star_is_rejected() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "
        CREATE TABLE t (id INT PRIMARY KEY);
        INSERT INTO t VALUES (1), (2);
    ",
    );
    let err = try_run(&mut e, "SELECT COUNT(DISTINCT *) FROM t").unwrap_err();
    assert!(err.contains("DISTINCT"), "{err}");
}

#[test]
fn create_if_not_exists_is_idempotent() {
    let mut e = MemoryEngine::new();
    run_all(&mut e, "CREATE TABLE t (a INT)");
    run_all(&mut e, "CREATE TABLE IF NOT EXISTS t (a INT)");
    run_all(&mut e, "CREATE TABLE IF NOT EXISTS t (a INT)");
    assert_eq!(e.list_tables().len(), 1);
}
