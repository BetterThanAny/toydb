//! End-to-end "kitchen sink" tests — exercise many features together.

use toydb::engine::MemoryEngine;
use toydb::executor::{Executor, ResultSet};
use toydb::sql::Parser;
use toydb::types::value::Value;

fn run(engine: &mut MemoryEngine, sql: &str) -> ResultSet {
    let stmt = Parser::parse_one(sql).unwrap();
    Executor::new(engine).execute(&stmt).unwrap()
}

fn run_all(engine: &mut MemoryEngine, sql: &str) {
    for stmt in Parser::parse_all(sql).unwrap() {
        Executor::new(engine).execute(&stmt).unwrap();
    }
}

fn select_rows(rs: &ResultSet) -> &[toydb::types::row::Row] {
    match rs {
        ResultSet::Select { rows, .. } => rows,
        _ => panic!("expected Select"),
    }
}

#[test]
fn full_feature_pipeline() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "
        CREATE TABLE products (
            id INT PRIMARY KEY,
            name TEXT NOT NULL UNIQUE,
            category TEXT,
            price FLOAT,
            stock INT DEFAULT 0
        );
        CREATE TABLE sales (
            id INT PRIMARY KEY,
            product_id INT,
            qty INT,
            sold_at INT
        );
        INSERT INTO products (id, name, category, price, stock) VALUES
            (1, 'Widget', 'tool', 9.99, 100),
            (2, 'Gizmo',  'tool', 19.99, 50),
            (3, 'Doodad', 'art',   3.50, 200),
            (4, 'Thingamajig', NULL, 7.25, 0);
        INSERT INTO sales VALUES
            (1, 1, 5,  100),
            (2, 1, 3,  101),
            (3, 2, 1,  102),
            (4, 3, 10, 100),
            (5, 3, 10, 101),
            (6, 1, 2,  103);
        ",
    );

    // ------ Aggregate over join with grouping ------
    let r = run(
        &mut e,
        "SELECT p.name, COUNT(*) AS sale_count, SUM(s.qty) AS total_qty
           FROM products p INNER JOIN sales s ON p.id = s.product_id
          GROUP BY p.name
         HAVING SUM(s.qty) >= 10
          ORDER BY total_qty DESC",
    );
    let rows = select_rows(&r);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::String("Doodad".into()));
    assert_eq!(rows[0][2], Value::Integer(20));
    assert_eq!(rows[1][0], Value::String("Widget".into()));
    assert_eq!(rows[1][2], Value::Integer(10));

    // ------ DISTINCT + COUNT(DISTINCT) ------
    let r = run(
        &mut e,
        "SELECT COUNT(*), COUNT(DISTINCT category) FROM products",
    );
    let rows = select_rows(&r);
    assert_eq!(rows[0][0], Value::Integer(4));
    // Distinct categories: 'tool', 'art' (NULL excluded).
    assert_eq!(rows[0][1], Value::Integer(2));

    // ------ CASE WHEN + arithmetic ------
    let r = run(
        &mut e,
        "SELECT name,
                CASE WHEN price < 5 THEN 'cheap'
                     WHEN price < 15 THEN 'mid'
                     ELSE 'pricey' END AS tier
           FROM products ORDER BY price",
    );
    let rows = select_rows(&r);
    assert_eq!(rows[0][1], Value::String("cheap".into()));
    assert_eq!(rows[3][1], Value::String("pricey".into()));

    // ------ LEFT JOIN: products with no sales ------
    let r = run(
        &mut e,
        "SELECT p.name, COUNT(s.id) AS sales_count
           FROM products p LEFT JOIN sales s ON p.id = s.product_id
          GROUP BY p.name
          ORDER BY sales_count DESC, p.name",
    );
    let rows = select_rows(&r);
    // Thingamajig (no sales) should be at the end.
    assert_eq!(rows.last().unwrap()[0], Value::String("Thingamajig".into()));
    assert_eq!(rows.last().unwrap()[1], Value::Integer(0));

    // ------ Transaction rollback ------
    run(&mut e, "BEGIN");
    run(&mut e, "DELETE FROM products WHERE id <= 2");
    let r = run(&mut e, "SELECT COUNT(*) FROM products");
    assert_eq!(select_rows(&r)[0][0], Value::Integer(2));
    run(&mut e, "ROLLBACK");
    let r = run(&mut e, "SELECT COUNT(*) FROM products");
    assert_eq!(select_rows(&r)[0][0], Value::Integer(4));

    // ------ NULLS handling in ORDER BY ------
    let r = run(
        &mut e,
        "SELECT name, category FROM products ORDER BY category NULLS FIRST",
    );
    let rows = select_rows(&r);
    // Thingamajig has NULL category — should be first.
    assert_eq!(rows[0][0], Value::String("Thingamajig".into()));

    // ------ EXPLAIN ------
    let r = run(
        &mut e,
        "EXPLAIN SELECT name FROM products WHERE price > 5 ORDER BY name LIMIT 3",
    );
    match r {
        ResultSet::Explain(plan) => {
            assert!(plan.contains("Scan"));
            assert!(plan.contains("Filter"));
            assert!(plan.contains("Sort"));
            assert!(plan.contains("Limit"));
        }
        _ => panic!(),
    }
}

#[test]
fn null_three_valued_logic() {
    let mut e = MemoryEngine::new();
    run_all(
        &mut e,
        "CREATE TABLE t (a INT, b INT); INSERT INTO t VALUES (1,1),(2,NULL),(NULL,3);",
    );

    // a = b: only matches when both non-null and equal → 1 row (1,1).
    let r = run(&mut e, "SELECT COUNT(*) FROM t WHERE a = b");
    assert_eq!(select_rows(&r)[0][0], Value::Integer(1));

    // a = b OR a IS NULL: adds 1 more (NULL,3) → 2 rows.
    let r = run(&mut e, "SELECT COUNT(*) FROM t WHERE a = b OR a IS NULL");
    assert_eq!(select_rows(&r)[0][0], Value::Integer(2));

    // a + b: NULL when either side is NULL.
    let r = run(&mut e, "SELECT a + b FROM t ORDER BY a NULLS FIRST");
    let rows = select_rows(&r);
    assert_eq!(rows[0][0], Value::Null);
    assert_eq!(rows[1][0], Value::Integer(2));
    assert_eq!(rows[2][0], Value::Null);
}
