//! End-to-end index tests: SQL DDL, plan selection, maintenance, and
//! disk reopen/rebuild behaviour.

use std::path::{Path, PathBuf};

use toydb::engine::{DiskEngine, Engine, MemoryEngine};
use toydb::executor::{Executor, ResultSet};
use toydb::sql::Parser;
use toydb::types::value::Value;

fn run(engine: &mut dyn Engine, sql: &str) -> ResultSet {
    let stmt = Parser::parse_one(sql).unwrap_or_else(|e| panic!("parse `{sql}`: {e}"));
    Executor::new(engine)
        .execute(&stmt)
        .unwrap_or_else(|e| panic!("exec `{sql}`: {e}"))
}

fn try_run(engine: &mut dyn Engine, sql: &str) -> Result<ResultSet, String> {
    let stmt = Parser::parse_one(sql).map_err(|e| e.to_string())?;
    Executor::new(engine)
        .execute(&stmt)
        .map_err(|e| e.to_string())
}

fn run_all(engine: &mut dyn Engine, sql: &str) {
    for stmt in Parser::parse_all(sql).unwrap() {
        Executor::new(engine).execute(&stmt).unwrap();
    }
}

fn select_rows(rs: ResultSet) -> Vec<toydb::types::row::Row> {
    match rs {
        ResultSet::Select { rows, .. } => rows,
        other => panic!("expected SELECT, got {other:?}"),
    }
}

fn explain(engine: &mut dyn Engine, sql: &str) -> String {
    match run(engine, sql) {
        ResultSet::Explain(plan) => plan,
        other => panic!("expected EXPLAIN, got {other:?}"),
    }
}

fn tmpdb() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("toydb-index-{}-{n}-{c}.db", std::process::id()))
}

fn cleanup(path: &Path) {
    std::fs::remove_file(path).ok();
    let mut wal = path.to_path_buf();
    let stem = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    wal.set_file_name(format!("{stem}-wal"));
    std::fs::remove_file(wal).ok();
}

#[test]
fn create_index_changes_explain_and_preserves_results() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "
        CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT);
        INSERT INTO users VALUES
            (1, 'alice', 20),
            (2, 'bob', 30),
            (3, 'carol', 20),
            (4, 'dave', 40);
    ",
    );

    let seq_plan = explain(&mut e, "EXPLAIN SELECT id, name FROM users WHERE age = 20");
    assert!(seq_plan.contains("SeqScan"), "{seq_plan}");
    let seq_rows = select_rows(run(
        &mut e,
        "SELECT id, name FROM users WHERE age = 20 ORDER BY id",
    ));

    run(&mut e, "CREATE INDEX idx_users_age ON users(age)");
    let index_plan = explain(&mut e, "EXPLAIN SELECT id, name FROM users WHERE age = 20");
    assert!(index_plan.contains("IndexScan"), "{index_plan}");
    assert!(index_plan.contains("index=idx_users_age"), "{index_plan}");
    assert!(index_plan.contains("key=20"), "{index_plan}");

    let index_rows = select_rows(run(
        &mut e,
        "SELECT id, name FROM users WHERE age = 20 ORDER BY id",
    ));
    assert_eq!(index_rows, seq_rows);

    run(&mut e, "DROP INDEX idx_users_age");
    let drop_plan = explain(&mut e, "EXPLAIN SELECT id, name FROM users WHERE age = 20");
    assert!(drop_plan.contains("SeqScan"), "{drop_plan}");
}

#[test]
fn disk_failed_create_index_does_not_leave_planner_ghost() {
    let path = tmpdb();
    {
        let mut e = DiskEngine::open(&path).unwrap();
        run_all(
            &mut e,
            "
            CREATE TABLE t (a INT, b INT);
            INSERT INTO t VALUES (1, 2);
        ",
        );

        let long_a = format!("idxa_{}", "a".repeat(7950));
        let long_b = format!("idxb_{}", "b".repeat(300));
        run(&mut e, &format!("CREATE INDEX {long_a} ON t(a)"));
        let err = try_run(&mut e, &format!("CREATE INDEX {long_b} ON t(b)")).unwrap_err();
        assert!(err.contains("catalog entry"), "{err}");

        let plan = explain(&mut e, "EXPLAIN SELECT a FROM t WHERE b = 2");
        assert!(plan.contains("SeqScan"), "{plan}");
        assert!(!plan.contains("IndexScan"), "{plan}");

        let rows = select_rows(run(&mut e, "SELECT a FROM t WHERE b = 2"));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], Value::Integer(1));
    }
    cleanup(&path);
}

#[test]
fn index_tracks_insert_update_and_delete() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "
        CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT);
        CREATE INDEX idx_users_age ON users(age);
        INSERT INTO users VALUES (1, 'alice', 20), (2, 'bob', 20), (3, 'carol', 30);
    ",
    );

    let rows = select_rows(run(
        &mut e,
        "SELECT id FROM users WHERE age = 20 ORDER BY id",
    ));
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::Integer(1));
    assert_eq!(rows[1][0], Value::Integer(2));

    run(&mut e, "UPDATE users SET age = 21 WHERE id = 1");
    let rows = select_rows(run(
        &mut e,
        "SELECT id FROM users WHERE age = 20 ORDER BY id",
    ));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Integer(2));
    let rows = select_rows(run(&mut e, "SELECT id FROM users WHERE age = 21"));
    assert_eq!(rows[0][0], Value::Integer(1));

    run(&mut e, "DELETE FROM users WHERE id = 2");
    let rows = select_rows(run(&mut e, "SELECT id FROM users WHERE age = 20"));
    assert!(rows.is_empty());
}

#[test]
fn index_scan_still_applies_full_where_predicate() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "
        CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT);
        INSERT INTO users VALUES
            (1, 'alice', 20),
            (2, 'bob', 20),
            (3, 'carol', 30);
        CREATE INDEX idx_users_age ON users(age);
    ",
    );

    let plan = explain(
        &mut e,
        "EXPLAIN SELECT id FROM users WHERE age = 20 AND name = 'bob'",
    );
    assert!(plan.contains("IndexScan"), "{plan}");
    let rows = select_rows(run(
        &mut e,
        "SELECT id FROM users WHERE age = 20 AND name = 'bob'",
    ));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Integer(2));
}

#[test]
fn null_equality_does_not_choose_index_scan() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "
        CREATE TABLE users (id INT PRIMARY KEY, age INT);
        INSERT INTO users VALUES (1, NULL), (2, 20);
        CREATE INDEX idx_users_age ON users(age);
    ",
    );

    let plan = explain(&mut e, "EXPLAIN SELECT id FROM users WHERE age = NULL");
    assert!(plan.contains("SeqScan"), "{plan}");
    assert!(!plan.contains("IndexScan"), "{plan}");
    let rows = select_rows(run(&mut e, "SELECT id FROM users WHERE age = NULL"));
    assert!(rows.is_empty());
}

#[test]
fn explain_update_delete_reports_seq_scan() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "
        CREATE TABLE users (id INT PRIMARY KEY, age INT);
        INSERT INTO users VALUES (1, 20), (2, 30);
        CREATE INDEX idx_users_age ON users(age);
    ",
    );

    let plan = explain(&mut e, "EXPLAIN UPDATE users SET age = 21 WHERE age = 20");
    assert!(
        plan.contains("Update `users` (seq scan, filtered)"),
        "{plan}"
    );
    let plan = explain(&mut e, "EXPLAIN DELETE FROM users WHERE age = 30");
    assert!(
        plan.contains("Delete from `users` (seq scan, filtered)"),
        "{plan}"
    );
}

#[test]
fn disk_index_metadata_survives_reopen_and_rebuilds_tree() {
    let path = tmpdb();
    {
        let mut e = DiskEngine::open(&path).unwrap();
        run_all(
            &mut e,
            "
            CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT);
            INSERT INTO users VALUES (1, 'alice', 20), (2, 'bob', 30), (3, 'carol', 20);
            CREATE INDEX idx_users_age ON users(age);
        ",
        );
        e.checkpoint().unwrap();
    }
    {
        let mut e = DiskEngine::open(&path).unwrap();
        let plan = explain(&mut e, "EXPLAIN SELECT name FROM users WHERE age = 20");
        assert!(plan.contains("IndexScan"), "{plan}");
        let rows = select_rows(run(
            &mut e,
            "SELECT name FROM users WHERE age = 20 ORDER BY name",
        ));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Value::String("alice".into()));
        assert_eq!(rows[1][0], Value::String("carol".into()));
    }
    cleanup(&path);
}

#[test]
fn disk_wal_replay_rebuilds_index_without_stale_entries() {
    let path = tmpdb();
    {
        let mut e = DiskEngine::open(&path).unwrap();
        run_all(
            &mut e,
            "
            CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT);
            CREATE INDEX idx_users_age ON users(age);
            INSERT INTO users VALUES (1, 'alice', 20), (2, 'bob', 30);
            UPDATE users SET age = 20 WHERE id = 2;
            DELETE FROM users WHERE id = 1;
        ",
        );
        // Simulate a process crash: reopen replays WAL, then rebuilds the
        // in-memory BTreeMap from recovered table pages.
        std::mem::forget(e);
    }
    {
        let mut e = DiskEngine::open(&path).unwrap();
        let plan = explain(&mut e, "EXPLAIN SELECT id FROM users WHERE age = 20");
        assert!(plan.contains("IndexScan"), "{plan}");
        let rows = select_rows(run(&mut e, "SELECT id FROM users WHERE age = 20"));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], Value::Integer(2));
        let rows = select_rows(run(&mut e, "SELECT id FROM users WHERE age = 30"));
        assert!(rows.is_empty());
    }
    cleanup(&path);
}
