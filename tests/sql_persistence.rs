//! End-to-end persistence: open a `DiskEngine`, write, close, reopen,
//! verify the data survived. Covers crash-free durability — crash tests
//! happen at the unit level (storage::wal).

use std::path::PathBuf;

use toydb::engine::{DiskEngine, Engine};
use toydb::executor::{Executor, ResultSet};
use toydb::sql::Parser;
use toydb::types::value::Value;

fn tmpdb() -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("toydb-it-{}-{n}.db", std::process::id()))
}

fn cleanup(p: &std::path::Path) {
    std::fs::remove_file(p).ok();
    let mut wal = p.to_path_buf();
    let stem = p
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    wal.set_file_name(format!("{stem}-wal"));
    std::fs::remove_file(&wal).ok();
}

fn run_all(engine: &mut DiskEngine, sql: &str) {
    for stmt in Parser::parse_all(sql).unwrap() {
        Executor::new(engine).execute(&stmt).unwrap();
    }
}

fn run(engine: &mut DiskEngine, sql: &str) -> ResultSet {
    let stmt = Parser::parse_one(sql).unwrap();
    Executor::new(engine).execute(&stmt).unwrap()
}

#[test]
fn ddl_and_data_survive_close_reopen() {
    let path = tmpdb();
    {
        let mut e = DiskEngine::open(&path).unwrap();
        run_all(&mut e, "
            CREATE TABLE notes (id INT PRIMARY KEY, body TEXT);
            INSERT INTO notes VALUES (1, 'hello'), (2, 'world');
        ");
        e.checkpoint().unwrap();
    }
    {
        let mut e = DiskEngine::open(&path).unwrap();
        let r = run(&mut e, "SELECT body FROM notes ORDER BY id");
        match r {
            ResultSet::Select { rows, .. } => {
                assert_eq!(rows[0][0], Value::String("hello".into()));
                assert_eq!(rows[1][0], Value::String("world".into()));
            }
            _ => panic!(),
        }
    }
    cleanup(&path);
}

#[test]
fn updates_and_deletes_survive() {
    let path = tmpdb();
    {
        let mut e = DiskEngine::open(&path).unwrap();
        run_all(&mut e, "
            CREATE TABLE k (id INT PRIMARY KEY, n INT);
            INSERT INTO k VALUES (1, 10), (2, 20), (3, 30);
            UPDATE k SET n = n + 1 WHERE id <= 2;
            DELETE FROM k WHERE id = 3;
        ");
        e.checkpoint().unwrap();
    }
    {
        let mut e = DiskEngine::open(&path).unwrap();
        let r = run(&mut e, "SELECT id, n FROM k ORDER BY id");
        match r {
            ResultSet::Select { rows, .. } => {
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0][1], Value::Integer(11));
                assert_eq!(rows[1][1], Value::Integer(21));
            }
            _ => panic!(),
        }
    }
    cleanup(&path);
}

#[test]
fn unique_constraint_persists() {
    let path = tmpdb();
    {
        let mut e = DiskEngine::open(&path).unwrap();
        run_all(&mut e, "
            CREATE TABLE u (id INT PRIMARY KEY, email TEXT UNIQUE);
            INSERT INTO u VALUES (1, 'a@x'), (2, 'b@x');
        ");
        e.checkpoint().unwrap();
    }
    {
        let mut e = DiskEngine::open(&path).unwrap();
        let stmt = Parser::parse_one("INSERT INTO u VALUES (3, 'a@x')").unwrap();
        let r = Executor::new(&mut e).execute(&stmt);
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("duplicate"));
    }
    cleanup(&path);
}

#[test]
fn many_inserts_span_pages() {
    let path = tmpdb();
    {
        let mut e = DiskEngine::open(&path).unwrap();
        run_all(&mut e, "CREATE TABLE big (id INT PRIMARY KEY, payload TEXT)");
        let big = "x".repeat(500);
        for i in 0..100 {
            let sql = format!("INSERT INTO big VALUES ({i}, '{big}')");
            run(&mut e, &sql);
        }
        e.checkpoint().unwrap();
    }
    {
        let mut e = DiskEngine::open(&path).unwrap();
        let r = run(&mut e, "SELECT COUNT(*) FROM big");
        match r {
            ResultSet::Select { rows, .. } => {
                assert_eq!(rows[0][0], Value::Integer(100));
            }
            _ => panic!(),
        }
    }
    cleanup(&path);
}

#[test]
fn drop_table_persists() {
    let path = tmpdb();
    {
        let mut e = DiskEngine::open(&path).unwrap();
        run_all(&mut e, "CREATE TABLE t (a INT PRIMARY KEY)");
        run_all(&mut e, "DROP TABLE t");
        e.checkpoint().unwrap();
    }
    {
        let e = DiskEngine::open(&path).unwrap();
        assert!(e.get_table("t").is_err());
    }
    cleanup(&path);
}
