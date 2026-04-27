//! Stress tests — bulk inserts, large scans, repeated updates. These
//! aren't strictly benchmarks (no `criterion`), but they establish a
//! baseline that the executor handles a few thousand rows without
//! exploding.

use std::time::Instant;

use toydb::engine::{DiskEngine, Engine, MemoryEngine};
use toydb::executor::{Executor, ResultSet};
use toydb::sql::Parser;
use toydb::types::value::Value;

fn run(engine: &mut dyn Engine, sql: &str) -> ResultSet {
    let stmt = Parser::parse_one(sql).unwrap();
    Executor::new(engine).execute(&stmt).unwrap()
}

fn run_all(engine: &mut dyn Engine, sql: &str) {
    for stmt in Parser::parse_all(sql).unwrap() {
        Executor::new(engine).execute(&stmt).unwrap();
    }
}

#[test]
fn memory_handles_5k_rows() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "CREATE TABLE big (id INT PRIMARY KEY, n INT, s TEXT)",
    );
    let t0 = Instant::now();
    for i in 0..5000 {
        let sql = format!("INSERT INTO big VALUES ({i}, {}, 'row{i}')", i * 2);
        run(&mut e, &sql);
    }
    let elapsed = t0.elapsed();
    eprintln!("inserted 5k rows in {:?}", elapsed);

    let t1 = Instant::now();
    let r = run(&mut e, "SELECT COUNT(*), SUM(n) FROM big WHERE n >= 100 AND n < 1000");
    let elapsed = t1.elapsed();
    eprintln!("aggregate in {:?}", elapsed);
    match r {
        ResultSet::Select { rows, .. } => {
            // n in [100, 1000) means i in [50, 500) → 450 rows.
            assert_eq!(rows[0][0], Value::Integer(450));
        }
        _ => panic!(),
    }
}

#[test]
fn memory_join_2k_x_2k() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "
        CREATE TABLE a (id INT PRIMARY KEY, val INT);
        CREATE TABLE b (id INT PRIMARY KEY, a_id INT, marker INT);
    ",
    );
    for i in 0..2000 {
        run(&mut e, &format!("INSERT INTO a VALUES ({i}, {})", i * 10));
        run(&mut e, &format!("INSERT INTO b VALUES ({i}, {i}, {})", i % 7));
    }
    let t = Instant::now();
    let r = run(
        &mut e,
        "SELECT COUNT(*) FROM a INNER JOIN b ON a.id = b.a_id WHERE b.marker = 0",
    );
    eprintln!("join+filter in {:?}", t.elapsed());
    match r {
        ResultSet::Select { rows, .. } => {
            // Every 7th row matches → ~286 (2000 / 7 rounded).
            let n = match rows[0][0] {
                Value::Integer(n) => n,
                _ => panic!(),
            };
            assert!((280..=290).contains(&n), "got {n}");
        }
        _ => panic!(),
    }
}

#[test]
fn disk_handles_2k_rows_round_trip() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let path = std::env::temp_dir().join(format!("toydb-stress-{n}.db"));
    {
        let mut e = DiskEngine::open(&path).unwrap();
        run_all(&mut e, "CREATE TABLE big (id INT PRIMARY KEY, n INT, s TEXT)");
        let t = Instant::now();
        for i in 0..2000 {
            run(&mut e, &format!("INSERT INTO big VALUES ({i}, {}, 'row{i}')", i * 2));
        }
        eprintln!("disk insert 2k rows in {:?}", t.elapsed());
        e.checkpoint().unwrap();
    }
    {
        let mut e = DiskEngine::open(&path).unwrap();
        let r = run(&mut e, "SELECT COUNT(*) FROM big");
        match r {
            ResultSet::Select { rows, .. } => assert_eq!(rows[0][0], Value::Integer(2000)),
            _ => panic!(),
        }
    }
    std::fs::remove_file(&path).ok();
    let mut wal = path.clone();
    wal.set_file_name(format!(
        "{}-wal",
        path.file_name().unwrap().to_string_lossy()
    ));
    std::fs::remove_file(&wal).ok();
}

#[test]
fn repeated_update_consistent() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "CREATE TABLE c (id INT PRIMARY KEY, v INT)",
    );
    for i in 0..100 {
        run(&mut e, &format!("INSERT INTO c VALUES ({i}, 0)"));
    }
    for _ in 0..50 {
        run(&mut e, "UPDATE c SET v = v + 1");
    }
    let r = run(&mut e, "SELECT COUNT(*), SUM(v) FROM c WHERE v = 50");
    match r {
        ResultSet::Select { rows, .. } => {
            assert_eq!(rows[0][0], Value::Integer(100));
            assert_eq!(rows[0][1], Value::Integer(5000));
        }
        _ => panic!(),
    }
}
