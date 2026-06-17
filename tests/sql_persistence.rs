//! End-to-end persistence: open a `DiskEngine`, write, close, reopen,
//! verify the data survived. Covers crash-free durability — crash tests
//! happen at the unit level (storage::wal).

use std::path::PathBuf;

use toydb::engine::{DiskEngine, Engine};
use toydb::executor::{Executor, ResultSet};
use toydb::sql::Parser;
use toydb::storage::PAGE_SIZE;
use toydb::storage::page::HEADER_SIZE;
use toydb::storage::wal::{LogRecord, Wal};
use toydb::types::value::Value;

fn tmpdb() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("toydb-it-{}-{n}-{c}.db", std::process::id()))
}

fn cleanup(p: &std::path::Path) {
    std::fs::remove_file(p).ok();
    std::fs::remove_file(wal_path(p)).ok();
}

fn wal_path(p: &std::path::Path) -> PathBuf {
    let mut wal = p.to_path_buf();
    let stem = p
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    wal.set_file_name(format!("{stem}-wal"));
    wal
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

fn try_run(engine: &mut DiskEngine, sql: &str) -> Result<ResultSet, String> {
    let stmt = Parser::parse_one(sql).map_err(|e| e.to_string())?;
    Executor::new(engine)
        .execute(&stmt)
        .map_err(|e| e.to_string())
}

#[test]
fn ddl_and_data_survive_close_reopen() {
    let path = tmpdb();
    {
        let mut e = DiskEngine::open(&path).unwrap();
        run_all(
            &mut e,
            "
            CREATE TABLE notes (id INT PRIMARY KEY, body TEXT);
            INSERT INTO notes VALUES (1, 'hello'), (2, 'world');
        ",
        );
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
        run_all(
            &mut e,
            "
            CREATE TABLE k (id INT PRIMARY KEY, n INT);
            INSERT INTO k VALUES (1, 10), (2, 20), (3, 30);
            UPDATE k SET n = n + 1 WHERE id <= 2;
            DELETE FROM k WHERE id = 3;
        ",
        );
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
fn disk_update_delete_write_statement_batch_wal_records() {
    let path = tmpdb();
    {
        let mut e = DiskEngine::open(&path).unwrap();
        run_all(
            &mut e,
            "
            CREATE TABLE t (id INT PRIMARY KEY, v INT);
            INSERT INTO t VALUES (1, 10), (2, 20), (3, 30);
        ",
        );
        e.checkpoint().unwrap();
        run_all(
            &mut e,
            "
            UPDATE t SET v = v + 100 WHERE id <= 2;
            DELETE FROM t WHERE id >= 2;
        ",
        );

        let mut wal = Wal::open(wal_path(&path)).unwrap();
        let recs = wal.replay().unwrap();
        assert!(
            recs.iter().any(|r| matches!(
                r,
                LogRecord::UpdateBatch { table, rows } if table == "t" && rows.len() == 2
            )),
            "{recs:?}"
        );
        assert!(
            recs.iter().any(|r| matches!(
                r,
                LogRecord::DeleteBatch { table, ids } if table == "t" && ids.len() == 2
            )),
            "{recs:?}"
        );
    }
    cleanup(&path);
}

#[test]
fn wal_replay_recovers_update_and_delete_batches() {
    let path = tmpdb();
    {
        let mut e = DiskEngine::open(&path).unwrap();
        run_all(
            &mut e,
            "
            CREATE TABLE t (id INT PRIMARY KEY, v INT);
            INSERT INTO t VALUES (1, 10), (2, 20), (3, 30);
        ",
        );
        e.checkpoint().unwrap();
        run_all(
            &mut e,
            "
            UPDATE t SET v = v + 100 WHERE id <= 2;
            DELETE FROM t WHERE id = 3;
        ",
        );
        std::mem::forget(e);
    }
    {
        let mut e = DiskEngine::open(&path).unwrap();
        let r = run(&mut e, "SELECT id, v FROM t ORDER BY id");
        match r {
            ResultSet::Select { rows, .. } => {
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0][0], Value::Integer(1));
                assert_eq!(rows[0][1], Value::Integer(110));
                assert_eq!(rows[1][0], Value::Integer(2));
                assert_eq!(rows[1][1], Value::Integer(120));
            }
            _ => panic!(),
        }
    }
    cleanup(&path);
}

#[test]
fn failed_oversized_update_keeps_old_row() {
    let path = tmpdb();
    {
        let mut e = DiskEngine::open(&path).unwrap();
        run_all(
            &mut e,
            "
            CREATE TABLE t (id INT PRIMARY KEY, payload TEXT);
            INSERT INTO t VALUES (1, 'ok');
        ",
        );
        let big = "x".repeat(9000);
        let err = try_run(
            &mut e,
            &format!("UPDATE t SET payload = '{big}' WHERE id = 1"),
        )
        .unwrap_err();
        assert!(
            err.contains("row for table")
                || err.contains("reallocation")
                || err.contains("page full")
        );

        let r = run(&mut e, "SELECT id, payload FROM t");
        match r {
            ResultSet::Select { rows, .. } => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0][0], Value::Integer(1));
                assert_eq!(rows[0][1], Value::String("ok".into()));
            }
            _ => panic!(),
        }
        e.checkpoint().unwrap();
    }
    {
        let mut e = DiskEngine::open(&path).unwrap();
        let r = run(&mut e, "SELECT id, payload FROM t");
        match r {
            ResultSet::Select { rows, .. } => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0][1], Value::String("ok".into()));
            }
            _ => panic!(),
        }
    }
    cleanup(&path);
}

#[test]
fn wal_replay_survives_delete_insert_slot_reuse() {
    let path = tmpdb();
    {
        let mut e = DiskEngine::open(&path).unwrap();
        run_all(
            &mut e,
            "
            CREATE TABLE t (id INT PRIMARY KEY, value TEXT);
            INSERT INTO t VALUES (1, 'old');
            DELETE FROM t WHERE id = 1;
            INSERT INTO t VALUES (2, 'new');
        ",
        );
        // Simulate a process crash: reopen must replay a WAL containing
        // insert/delete/insert records for the same physical slot.
        std::mem::forget(e);
    }
    {
        let mut e = DiskEngine::open(&path).unwrap();
        let r = run(&mut e, "SELECT id, value FROM t ORDER BY id");
        match r {
            ResultSet::Select { rows, .. } => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0][0], Value::Integer(2));
                assert_eq!(rows[0][1], Value::String("new".into()));
            }
            _ => panic!(),
        }
    }
    cleanup(&path);
}

#[test]
fn wal_replay_survives_drop_recreate_same_table_name() {
    let path = tmpdb();
    {
        let mut e = DiskEngine::open(&path).unwrap();
        run_all(
            &mut e,
            "
            CREATE TABLE t (id INT PRIMARY KEY, old_col INT);
            CREATE INDEX idx_t_old ON t(old_col);
            INSERT INTO t VALUES (1, 10);
            DROP TABLE t;
            CREATE TABLE t (id INT PRIMARY KEY, new_col TEXT);
            INSERT INTO t VALUES (1, 'new');
        ",
        );
        // Simulate a process crash: replay must not apply old table/index
        // records to the later table incarnation with the same name.
        std::mem::forget(e);
    }
    {
        let mut e = DiskEngine::open(&path).unwrap();
        let r = run(&mut e, "SELECT id, new_col FROM t ORDER BY id");
        match r {
            ResultSet::Select { rows, .. } => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0][0], Value::Integer(1));
                assert_eq!(rows[0][1], Value::String("new".into()));
            }
            _ => panic!(),
        }
        let err = try_run(&mut e, "SELECT old_col FROM t").unwrap_err();
        assert!(err.contains("no such column `old_col`"), "{err}");
    }
    cleanup(&path);
}

#[test]
fn wal_replay_recovers_multi_value_insert_as_batch() {
    let path = tmpdb();
    {
        let mut e = DiskEngine::open(&path).unwrap();
        run_all(
            &mut e,
            "CREATE TABLE big (id INT PRIMARY KEY, payload TEXT)",
        );
        let payload = "x".repeat(500);
        let mut insert = String::from("INSERT INTO big VALUES ");
        for i in 0..40 {
            if i > 0 {
                insert.push(',');
            }
            insert.push_str(&format!("({i}, '{payload}')"));
        }
        insert.push(';');
        run_all(&mut e, &insert);
        // Simulate a process crash before the clean-exit checkpoint truncates
        // the batch WAL record.
        std::mem::forget(e);
    }
    {
        let mut e = DiskEngine::open(&path).unwrap();
        let r = run(&mut e, "SELECT COUNT(*) FROM big");
        match r {
            ResultSet::Select { rows, .. } => {
                assert_eq!(rows[0][0], Value::Integer(40));
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
        run_all(
            &mut e,
            "
            CREATE TABLE u (id INT PRIMARY KEY, email TEXT UNIQUE);
            INSERT INTO u VALUES (1, 'a@x'), (2, 'b@x');
        ",
        );
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
fn disk_update_can_swap_unique_values_and_persist() {
    let path = tmpdb();
    {
        let mut e = DiskEngine::open(&path).unwrap();
        run_all(
            &mut e,
            "
            CREATE TABLE u (id INT PRIMARY KEY, v INT UNIQUE);
            INSERT INTO u VALUES (1, 100), (2, 200);
            UPDATE u SET v = CASE WHEN v = 100 THEN 200 ELSE 100 END;
        ",
        );
        e.checkpoint().unwrap();
    }
    {
        let mut e = DiskEngine::open(&path).unwrap();
        let r = run(&mut e, "SELECT id, v FROM u ORDER BY id");
        match r {
            ResultSet::Select { rows, .. } => {
                assert_eq!(rows[0][1], Value::Integer(200));
                assert_eq!(rows[1][1], Value::Integer(100));
            }
            _ => panic!(),
        }
    }
    cleanup(&path);
}

#[test]
fn disk_multi_value_insert_failure_is_atomic() {
    let path = tmpdb();
    {
        let mut e = DiskEngine::open(&path).unwrap();
        run_all(
            &mut e,
            "
            CREATE TABLE u (id INT PRIMARY KEY, email TEXT UNIQUE);
            INSERT INTO u VALUES (1, 'a@x'), (2, 'b@x');
        ",
        );
        let err = try_run(&mut e, "INSERT INTO u VALUES (3, 'c@x'), (4, 'a@x')").unwrap_err();
        assert!(err.contains("duplicate"));
        e.checkpoint().unwrap();
    }
    {
        let mut e = DiskEngine::open(&path).unwrap();
        let r = run(&mut e, "SELECT id FROM u ORDER BY id");
        match r {
            ResultSet::Select { rows, .. } => {
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[1][0], Value::Integer(2));
            }
            _ => panic!(),
        }
    }
    cleanup(&path);
}

#[test]
fn disk_update_page_capacity_failure_is_atomic() {
    let path = tmpdb();
    {
        let mut e = DiskEngine::open(&path).unwrap();
        let mut setup =
            String::from("CREATE TABLE t (id INT PRIMARY KEY, s TEXT); INSERT INTO t VALUES ");
        for i in 1..=40 {
            if i > 1 {
                setup.push(',');
            }
            setup.push_str(&format!("({i}, REPEAT('a', 200))"));
        }
        setup.push(';');
        run_all(&mut e, &setup);

        let err = try_run(
            &mut e,
            "UPDATE t SET s = REPEAT('x', 7000) WHERE id = 37 OR id = 1",
        )
        .unwrap_err();
        assert!(
            err.contains("reallocation") || err.contains("page full"),
            "{err}"
        );

        let r = run(
            &mut e,
            "SELECT id, LENGTH(s) FROM t WHERE id = 1 OR id = 37 ORDER BY id",
        );
        match r {
            ResultSet::Select { rows, .. } => {
                assert_eq!(rows[0][0], Value::Integer(1));
                assert_eq!(rows[0][1], Value::Integer(200));
                assert_eq!(rows[1][0], Value::Integer(37));
                assert_eq!(rows[1][1], Value::Integer(200));
            }
            _ => panic!(),
        }
        e.checkpoint().unwrap();
    }
    cleanup(&path);
}

#[test]
fn disk_expression_default_survives_reopen() {
    let path = tmpdb();
    {
        let mut e = DiskEngine::open(&path).unwrap();
        run_all(
            &mut e,
            "
            CREATE TABLE t (id INT PRIMARY KEY, x INT DEFAULT 1 + 1);
            INSERT INTO t (id) VALUES (1);
        ",
        );
        e.checkpoint().unwrap();
    }
    {
        let mut e = DiskEngine::open(&path).unwrap();
        run_all(&mut e, "INSERT INTO t (id) VALUES (2)");
        let r = run(&mut e, "SELECT id, x FROM t ORDER BY id");
        match r {
            ResultSet::Select { rows, .. } => {
                assert_eq!(rows[0][1], Value::Integer(2));
                assert_eq!(rows[1][1], Value::Integer(2));
            }
            _ => panic!(),
        }
    }
    cleanup(&path);
}

#[test]
fn wal_replay_ignores_and_truncates_torn_tail_after_valid_records() {
    use std::io::Write;

    let path = tmpdb();
    {
        let mut e = DiskEngine::open(&path).unwrap();
        run_all(&mut e, "CREATE TABLE t (id INT PRIMARY KEY)");
        // Simulate a process crash: keep a valid WAL record before the
        // torn tail below.
        std::mem::forget(e);
    }

    {
        let mut wal = std::fs::OpenOptions::new()
            .append(true)
            .open(wal_path(&path))
            .unwrap();
        wal.write_all(&10u32.to_le_bytes()).unwrap();
        wal.write_all(&[3]).unwrap();
        wal.sync_data().unwrap();
    }

    {
        let mut e = DiskEngine::open(&path).unwrap();
        let r = run(&mut e, "SELECT COUNT(*) FROM t");
        match r {
            ResultSet::Select { rows, .. } => assert_eq!(rows[0][0], Value::Integer(0)),
            _ => panic!(),
        }
    }
    assert_eq!(std::fs::metadata(wal_path(&path)).unwrap().len(), 0);
    cleanup(&path);
}

#[test]
fn corrupted_page_header_returns_error_instead_of_panicking() {
    use std::io::{Seek, SeekFrom, Write};

    let path = tmpdb();
    {
        let mut e = DiskEngine::open(&path).unwrap();
        run_all(&mut e, "CREATE TABLE t (id INT PRIMARY KEY)");
        e.checkpoint().unwrap();
    }

    let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    f.seek(SeekFrom::Start((2 * PAGE_SIZE + 4) as u64)).unwrap();
    f.write_all(&2000u32.to_le_bytes()).unwrap();

    let err = match DiskEngine::open(&path) {
        Ok(_) => panic!("corrupted database opened successfully"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("invalid page slot count"), "{err}");
    cleanup(&path);
}

#[test]
fn corrupted_super_page_count_returns_error() {
    use std::io::{Seek, SeekFrom, Write};

    let path = tmpdb();
    {
        let mut e = DiskEngine::open(&path).unwrap();
        e.checkpoint().unwrap();
    }

    let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    f.seek(SeekFrom::Start((HEADER_SIZE + 12) as u64)).unwrap();
    f.write_all(&0u64.to_le_bytes()).unwrap();
    f.sync_data().unwrap();

    let err = match DiskEngine::open(&path) {
        Ok(_) => panic!("corrupted database opened successfully"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("page_count is zero"), "{err}");
    cleanup(&path);
}

#[test]
fn corrupted_catalog_cycle_returns_error() {
    use std::io::{Read, Seek, SeekFrom, Write};

    let path = tmpdb();
    {
        let mut e = DiskEngine::open(&path).unwrap();
        run_all(&mut e, "CREATE TABLE t (id INT PRIMARY KEY)");
        e.checkpoint().unwrap();
    }

    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let mut root = [0; 8];
    f.seek(SeekFrom::Start((HEADER_SIZE + 28) as u64)).unwrap();
    f.read_exact(&mut root).unwrap();
    let root = u64::from_le_bytes(root);
    assert_ne!(root, 0);
    f.seek(SeekFrom::Start(root * PAGE_SIZE as u64 + 12))
        .unwrap();
    f.write_all(&root.to_le_bytes()).unwrap();
    f.sync_data().unwrap();

    let err = match DiskEngine::open(&path) {
        Ok(_) => panic!("corrupted database opened successfully"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("catalog page chain contains a cycle"), "{err}");
    cleanup(&path);
}

#[test]
fn many_inserts_span_pages() {
    let path = tmpdb();
    {
        let mut e = DiskEngine::open(&path).unwrap();
        run_all(
            &mut e,
            "CREATE TABLE big (id INT PRIMARY KEY, payload TEXT)",
        );
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

#[test]
fn wal_replay_recovers_drop_table_pages() {
    let path = tmpdb();
    {
        let mut e = DiskEngine::open(&path).unwrap();
        run_all(
            &mut e,
            "CREATE TABLE big (id INT PRIMARY KEY, payload TEXT)",
        );
        let payload = "x".repeat(500);
        for i in 0..40 {
            run(
                &mut e,
                &format!("INSERT INTO big VALUES ({i}, '{payload}')"),
            );
        }
        e.checkpoint().unwrap();
        run_all(&mut e, "DROP TABLE big");
        std::mem::forget(e);
    }
    {
        let e = DiskEngine::open(&path).unwrap();
        assert!(e.get_table("big").is_err());
    }
    cleanup(&path);
}
