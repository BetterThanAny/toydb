//! Statement executor.
//!
//! The executor is the bridge between the parsed [`Statement`] tree and
//! the [`Engine`] backend. Single-table queries are handled in this
//! module; the M7 work folds in joins, group-by, and aggregates.

use std::cmp::Ordering;
use std::collections::HashMap;

use crate::catalog::Table;
use crate::engine::{Engine, RowId};
use crate::error::{Error, Result};
use crate::executor::expr::{eval, eval_with, Resolver};
use crate::executor::result::{Column as ResultColumn, ResultSet};
use crate::sql::ast::{
    BinaryOp, ColumnDef, CreateTableStmt, DeleteStmt, DropTableStmt, Expression, FromClause,
    InsertStmt, JoinKind, OrderBy, SelectItem, SelectStmt, Statement, UnaryOp, UpdateStmt,
};
use crate::types::row::Row;
use crate::types::value::Value;

pub struct Executor<'a> {
    engine: &'a mut dyn Engine,
}

impl<'a> Executor<'a> {
    pub fn new(engine: &'a mut dyn Engine) -> Self { Self { engine } }

    /// Run one statement against the engine.
    pub fn execute(&mut self, stmt: &Statement) -> Result<ResultSet> {
        match stmt {
            Statement::CreateTable(s) => self.exec_create_table(s),
            Statement::DropTable(s) => self.exec_drop_table(s),
            Statement::Insert(s) => self.exec_insert(s),
            Statement::Select(s) => self.exec_select(s),
            Statement::Update(s) => self.exec_update(s),
            Statement::Delete(s) => self.exec_delete(s),
            Statement::Begin => Err(Error::other("BEGIN: transactions land in M9")),
            Statement::Commit => Err(Error::other("COMMIT: transactions land in M9")),
            Statement::Rollback => Err(Error::other("ROLLBACK: transactions land in M9")),
            Statement::Explain(_) => Err(Error::other("EXPLAIN is not implemented yet")),
        }
    }

    // ------------------------------------------------------------------
    // DDL
    // ------------------------------------------------------------------

    fn exec_create_table(&mut self, c: &CreateTableStmt) -> Result<ResultSet> {
        if self.engine.list_tables().iter().any(|n| n == &c.name) {
            if c.if_not_exists {
                return Ok(ResultSet::CreateTable { name: c.name.clone() });
            }
            return Err(Error::schema(format!("table `{}` already exists", c.name)));
        }
        let columns: Vec<crate::catalog::Column> = c.columns.iter().map(Into::into).collect();
        let table = Table::new(&c.name, columns)?;
        // Validate any DEFAULT expressions are constant-foldable.
        for col in &table.columns {
            if let Some(expr) = &col.default {
                eval(expr).map_err(|e| {
                    Error::schema(format!(
                        "DEFAULT for column `{}` is not a constant: {e}",
                        col.name
                    ))
                })?;
            }
        }
        self.engine.create_table(table)?;
        Ok(ResultSet::CreateTable { name: c.name.clone() })
    }

    fn exec_drop_table(&mut self, d: &DropTableStmt) -> Result<ResultSet> {
        let existed = self.engine.drop_table(&d.name, d.if_exists)?;
        Ok(ResultSet::DropTable { name: d.name.clone(), existed })
    }

    // ------------------------------------------------------------------
    // INSERT
    // ------------------------------------------------------------------

    fn exec_insert(&mut self, i: &InsertStmt) -> Result<ResultSet> {
        let table = self.engine.get_table(&i.table)?.clone();
        let target_indices: Vec<usize> = match &i.columns {
            None => (0..table.columns.len()).collect(),
            Some(names) => names.iter().map(|n| table.column_index(n)).collect::<Result<_>>()?,
        };
        let mut count = 0usize;
        for raw_row in &i.rows {
            if raw_row.len() != target_indices.len() {
                return Err(Error::ty(format!(
                    "INSERT into {}: expected {} values, got {}",
                    i.table,
                    target_indices.len(),
                    raw_row.len()
                )));
            }
            let mut full = vec![Value::Null; table.columns.len()];
            // Fill defaults / NULL for unspecified columns.
            for (idx, col) in table.columns.iter().enumerate() {
                if target_indices.contains(&idx) {
                    continue;
                }
                full[idx] = match &col.default {
                    Some(e) => eval(e)?,
                    None => Value::Null,
                };
            }
            // Evaluate the supplied expressions and place at the right index.
            for (slot, expr) in target_indices.iter().zip(raw_row.iter()) {
                full[*slot] = eval(expr)?;
            }
            self.engine.insert(&i.table, Row(full))?;
            count += 1;
        }
        Ok(ResultSet::Insert { count })
    }

    // ------------------------------------------------------------------
    // SELECT
    // ------------------------------------------------------------------

    fn exec_select(&mut self, s: &SelectStmt) -> Result<ResultSet> {
        // Aggregate / group-by / multi-table queries are not supported in
        // M5 — they land in M7.
        if !s.group_by.is_empty() {
            return Err(Error::other("GROUP BY: implemented in M7"));
        }
        if s.having.is_some() {
            return Err(Error::other("HAVING: implemented in M7"));
        }
        if let Some(FromClause::Join { .. }) = s.from {
            return Err(Error::other("JOIN: implemented in M7"));
        }

        let table_name = match &s.from {
            None => {
                // Constant query: each item evaluated with EmptyResolver.
                return self.exec_const_select(s);
            }
            Some(FromClause::Table { name, .. }) => name.clone(),
            Some(FromClause::Join { .. }) => unreachable!("rejected above"),
        };
        let alias = match &s.from {
            Some(FromClause::Table { alias, .. }) => alias.clone(),
            _ => None,
        };
        let table = self.engine.get_table(&table_name)?.clone();
        let rows = self.engine.scan(&table_name)?;

        // Reject aggregate functions in scalar context (they need GROUP BY).
        for item in &s.items {
            if let SelectItem::Expr { expr, .. } = item
                && expression_contains_aggregate(expr)
            {
                return Err(Error::other(
                    "aggregate without GROUP BY: implemented in M7",
                ));
            }
        }

        let alias_str = alias.as_deref().unwrap_or(&table_name);

        // -- WHERE -----------------------------------------------------
        let mut filtered: Vec<(RowId, Row)> = rows
            .into_iter()
            .filter_map(|(id, r)| match &s.r#where {
                None => Some(Ok((id, r))),
                Some(predicate) => {
                    let resolver = SingleTable { table: &table, alias: alias_str, row: &r };
                    match eval_with(predicate, &resolver) {
                        Ok(Value::Boolean(true)) => Some(Ok((id, r))),
                        Ok(Value::Boolean(false)) | Ok(Value::Null) => None,
                        Ok(other) => Some(Err(Error::ty(format!(
                            "WHERE expects boolean, got {}",
                            other.type_name()
                        )))),
                        Err(e) => Some(Err(e)),
                    }
                }
            })
            .collect::<Result<_>>()?;

        // -- ORDER BY --------------------------------------------------
        if !s.order_by.is_empty() {
            sort_rows(&mut filtered, &s.order_by, &table, alias_str)?;
        }

        // -- LIMIT / OFFSET -------------------------------------------
        let offset = match &s.offset {
            None => 0usize,
            Some(e) => limit_count(e, "OFFSET")?,
        };
        let limit = match &s.limit {
            None => usize::MAX,
            Some(e) => limit_count(e, "LIMIT")?,
        };
        // -- Project ---------------------------------------------------
        let (columns, project) = build_projection(&s.items, &table, alias_str)?;
        let mut output = Vec::new();
        for (_, row) in filtered.into_iter().skip(offset).take(limit) {
            let resolver = SingleTable { table: &table, alias: alias_str, row: &row };
            let mut out = Vec::with_capacity(project.len());
            for proj in &project {
                let v = match proj {
                    Projection::Column(idx) => row.0[*idx].clone(),
                    Projection::Expression(expr) => eval_with(expr, &resolver)?,
                };
                out.push(v);
            }
            output.push(Row(out));
        }
        Ok(ResultSet::Select { columns, rows: output })
    }

    /// Constant SELECT (`SELECT 1+1`) — no FROM, single output row.
    fn exec_const_select(&mut self, s: &SelectStmt) -> Result<ResultSet> {
        if s.r#where.is_some() || !s.order_by.is_empty() || s.limit.is_some() {
            return Err(Error::other("WHERE/ORDER/LIMIT need a FROM clause"));
        }
        let mut columns = Vec::with_capacity(s.items.len());
        let mut row = Vec::with_capacity(s.items.len());
        for (i, item) in s.items.iter().enumerate() {
            let (name, value) = match item {
                SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                    return Err(Error::other("`*` requires a FROM clause"));
                }
                SelectItem::Expr { expr, alias } => {
                    let v = eval(expr)?;
                    let n = alias
                        .clone()
                        .unwrap_or_else(|| format!("col{}", i + 1));
                    (n, v)
                }
            };
            columns.push(ResultColumn::new(name));
            row.push(value);
        }
        Ok(ResultSet::Select { columns, rows: vec![Row(row)] })
    }

    // ------------------------------------------------------------------
    // UPDATE
    // ------------------------------------------------------------------

    fn exec_update(&mut self, u: &UpdateStmt) -> Result<ResultSet> {
        let table = self.engine.get_table(&u.table)?.clone();
        let scan = self.engine.scan(&u.table)?;

        // Resolve assignment column indices once.
        let mut assignments: Vec<(usize, &Expression)> = Vec::with_capacity(u.assignments.len());
        for (col, expr) in &u.assignments {
            let idx = table.column_index(col)?;
            assignments.push((idx, expr));
        }

        // Snapshot first, then apply, so repeated UPDATE doesn't see its
        // own writes mid-statement.
        let mut to_update: Vec<(RowId, Row)> = Vec::new();
        for (id, row) in scan {
            let resolver = SingleTable { table: &table, alias: &u.table, row: &row };
            let take = match &u.r#where {
                None => true,
                Some(predicate) => match eval_with(predicate, &resolver)? {
                    Value::Boolean(true) => true,
                    Value::Boolean(false) | Value::Null => false,
                    other => return Err(Error::ty(format!(
                        "WHERE expects boolean, got {}",
                        other.type_name()
                    ))),
                },
            };
            if !take { continue; }
            // Compute new values referencing the *current* row (so
            // `SET a = a + 1` reads pre-update `a`).
            let mut new_row = row.clone();
            for (idx, expr) in &assignments {
                new_row.0[*idx] = eval_with(expr, &resolver)?;
            }
            to_update.push((id, new_row));
        }
        let count = to_update.len();
        for (id, new) in to_update {
            self.engine.update(&u.table, id, new)?;
        }
        Ok(ResultSet::Update { count })
    }

    // ------------------------------------------------------------------
    // DELETE
    // ------------------------------------------------------------------

    fn exec_delete(&mut self, d: &DeleteStmt) -> Result<ResultSet> {
        let table = self.engine.get_table(&d.table)?.clone();
        let scan = self.engine.scan(&d.table)?;
        let mut victims: Vec<RowId> = Vec::new();
        for (id, row) in scan {
            let take = match &d.r#where {
                None => true,
                Some(predicate) => {
                    let resolver = SingleTable { table: &table, alias: &d.table, row: &row };
                    match eval_with(predicate, &resolver)? {
                        Value::Boolean(true) => true,
                        Value::Boolean(false) | Value::Null => false,
                        other => return Err(Error::ty(format!(
                            "WHERE expects boolean, got {}",
                            other.type_name()
                        ))),
                    }
                }
            };
            if take { victims.push(id); }
        }
        let count = victims.len();
        for id in victims {
            self.engine.delete(&d.table, id)?;
        }
        Ok(ResultSet::Delete { count })
    }
}

// ---------------------------------------------------------------------
// Single-table column resolver
// ---------------------------------------------------------------------

struct SingleTable<'a> {
    table: &'a Table,
    alias: &'a str,
    row: &'a Row,
}

impl<'a> Resolver for SingleTable<'a> {
    fn column(&self, name: &str) -> Result<Value> {
        let idx = self.table.column_index(name)?;
        Ok(self.row.0[idx].clone())
    }
    fn qualified(&self, table: &str, name: &str) -> Result<Value> {
        if table != self.alias && table != self.table.name {
            return Err(Error::schema(format!(
                "no such alias `{table}` (have `{}`)",
                self.alias
            )));
        }
        let idx = self.table.column_index(name)?;
        Ok(self.row.0[idx].clone())
    }
}

// ---------------------------------------------------------------------
// Projection plan
// ---------------------------------------------------------------------

enum Projection<'a> {
    Column(usize),
    Expression(&'a Expression),
}

fn build_projection<'a>(
    items: &'a [SelectItem],
    table: &Table,
    alias: &str,
) -> Result<(Vec<ResultColumn>, Vec<Projection<'a>>)> {
    let mut columns = Vec::new();
    let mut plan = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard => {
                for col in &table.columns {
                    columns.push(ResultColumn::new(col.name.clone()));
                    plan.push(Projection::Column(table.column_index(&col.name)?));
                }
            }
            SelectItem::QualifiedWildcard(t) => {
                if t != alias && t != table.name.as_str() {
                    return Err(Error::schema(format!(
                        "qualified wildcard `{t}.*` does not match alias `{alias}`"
                    )));
                }
                for col in &table.columns {
                    columns.push(ResultColumn::new(col.name.clone()));
                    plan.push(Projection::Column(table.column_index(&col.name)?));
                }
            }
            SelectItem::Expr { expr, alias: out_alias } => {
                let display = match (out_alias, expr) {
                    (Some(a), _) => a.clone(),
                    (None, Expression::Column(c)) => c.clone(),
                    (None, Expression::Qualified(_, c)) => c.clone(),
                    (None, _) => "expr".to_string(),
                };
                columns.push(ResultColumn::new(display));
                plan.push(Projection::Expression(expr));
            }
        }
    }
    if columns.is_empty() {
        return Err(Error::schema("SELECT requires at least one item"));
    }
    // Disambiguate same-name columns by appending `:n`. Simple but stable.
    let mut counts: HashMap<String, usize> = HashMap::new();
    for c in &mut columns {
        let n = counts.entry(c.name.clone()).or_insert(0);
        if *n > 0 {
            c.name = format!("{}:{}", c.name, *n);
        }
        *n += 1;
    }
    Ok((columns, plan))
}

fn sort_rows(
    rows: &mut [(RowId, Row)],
    order_by: &[OrderBy],
    table: &Table,
    alias: &str,
) -> Result<()> {
    // Pre-compute sort keys to avoid re-evaluating expressions during cmp.
    struct Keyed { key: Vec<(Value, bool)>, payload: (RowId, Row) }
    let mut keyed: Vec<Keyed> = Vec::with_capacity(rows.len());
    // Drain `rows` into `keyed`. We have to take ownership to clone each
    // row, but that's fine for memory engine (which copies on scan anyway).
    for (id, row) in rows.iter() {
        let resolver = SingleTable { table, alias, row };
        let mut k = Vec::with_capacity(order_by.len());
        for ob in order_by {
            let v = eval_with(&ob.expr, &resolver)?;
            k.push((v, ob.asc));
        }
        keyed.push(Keyed { key: k, payload: (*id, row.clone()) });
    }
    keyed.sort_by(|a, b| {
        for ((av, asc), (bv, _)) in a.key.iter().zip(b.key.iter()) {
            let cmp = av.total_cmp(bv);
            if cmp != Ordering::Equal {
                return if *asc { cmp } else { cmp.reverse() };
            }
        }
        Ordering::Equal
    });
    for (i, k) in keyed.into_iter().enumerate() {
        rows[i] = k.payload;
    }
    Ok(())
}

fn limit_count(expr: &Expression, label: &str) -> Result<usize> {
    let v = eval(expr)?;
    match v {
        Value::Integer(n) if n >= 0 => Ok(n as usize),
        Value::Integer(n) => Err(Error::value(format!("{label} must be non-negative, got {n}"))),
        Value::Null => Ok(0),
        other => Err(Error::ty(format!("{label} expects integer, got {}", other.type_name()))),
    }
}

fn expression_contains_aggregate(e: &Expression) -> bool {
    match e {
        Expression::Function { name, .. } => {
            matches!(name.to_ascii_uppercase().as_str(),
                "COUNT" | "SUM" | "AVG" | "MIN" | "MAX")
        }
        Expression::Unary(_, inner) => expression_contains_aggregate(inner),
        Expression::Binary(l, _, r) => {
            expression_contains_aggregate(l) || expression_contains_aggregate(r)
        }
        Expression::IsNull { expr, .. } => expression_contains_aggregate(expr),
        Expression::InList { expr, list, .. } => {
            expression_contains_aggregate(expr) || list.iter().any(expression_contains_aggregate)
        }
        Expression::Between { expr, low, high, .. } => {
            expression_contains_aggregate(expr)
                || expression_contains_aggregate(low)
                || expression_contains_aggregate(high)
        }
        Expression::Like { expr, pattern, .. } => {
            expression_contains_aggregate(expr) || expression_contains_aggregate(pattern)
        }
        _ => false,
    }
}

// (Unused helpers left visible for the wider crate; suppress dead-code
// because some are touched only from tests in this milestone.)
#[allow(dead_code)]
fn _force_use_unaryop(_: UnaryOp, _: BinaryOp, _: ColumnDef, _: JoinKind) {}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::MemoryEngine;
    use crate::sql::Parser;

    fn run(engine: &mut MemoryEngine, sql: &str) -> Result<ResultSet> {
        let stmt = Parser::parse_one(sql)?;
        Executor::new(engine).execute(&stmt)
    }

    fn run_all(engine: &mut MemoryEngine, sql: &str) -> Vec<ResultSet> {
        let stmts = Parser::parse_all(sql).unwrap();
        stmts
            .iter()
            .map(|s| Executor::new(engine).execute(s).unwrap())
            .collect()
    }

    #[test]
    fn create_insert_select_basic() {
        let mut e = MemoryEngine::new();
        run(&mut e, "CREATE TABLE t (id INT PRIMARY KEY, n STRING)").unwrap();
        run(&mut e, "INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')").unwrap();
        let r = run(&mut e, "SELECT * FROM t").unwrap();
        match r {
            ResultSet::Select { columns, rows } => {
                assert_eq!(columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
                    vec!["id".to_string(), "n".to_string()]);
                assert_eq!(rows.len(), 3);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn select_const() {
        let mut e = MemoryEngine::new();
        let r = run(&mut e, "SELECT 1 + 1").unwrap();
        match r {
            ResultSet::Select { rows, .. } => {
                assert_eq!(rows[0][0], Value::Integer(2));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn insert_with_explicit_columns() {
        let mut e = MemoryEngine::new();
        run(&mut e, "CREATE TABLE t (a INT, b INT, c INT DEFAULT 99)").unwrap();
        run(&mut e, "INSERT INTO t (b, a) VALUES (20, 10)").unwrap();
        let r = run(&mut e, "SELECT * FROM t").unwrap();
        match r {
            ResultSet::Select { rows, .. } => {
                assert_eq!(rows[0][0], Value::Integer(10));
                assert_eq!(rows[0][1], Value::Integer(20));
                assert_eq!(rows[0][2], Value::Integer(99));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn where_filter() {
        let mut e = MemoryEngine::new();
        run(&mut e, "CREATE TABLE t (id INT PRIMARY KEY, age INT)").unwrap();
        run(&mut e, "INSERT INTO t VALUES (1, 18), (2, 25), (3, 12)").unwrap();
        let r = run(&mut e, "SELECT id FROM t WHERE age >= 18").unwrap();
        match r {
            ResultSet::Select { rows, .. } => assert_eq!(rows.len(), 2),
            _ => panic!(),
        }
    }

    #[test]
    fn order_by_asc_desc() {
        let mut e = MemoryEngine::new();
        run(&mut e, "CREATE TABLE t (id INT PRIMARY KEY, age INT)").unwrap();
        run(&mut e, "INSERT INTO t VALUES (1, 18), (2, 25), (3, 12)").unwrap();
        let r = run(&mut e, "SELECT age FROM t ORDER BY age").unwrap();
        match r {
            ResultSet::Select { rows, .. } => {
                assert_eq!(rows[0][0], Value::Integer(12));
                assert_eq!(rows[2][0], Value::Integer(25));
            }
            _ => panic!(),
        }
        let r = run(&mut e, "SELECT age FROM t ORDER BY age DESC").unwrap();
        match r {
            ResultSet::Select { rows, .. } => {
                assert_eq!(rows[0][0], Value::Integer(25));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn limit_offset() {
        let mut e = MemoryEngine::new();
        run(&mut e, "CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
        run(&mut e, "INSERT INTO t VALUES (1), (2), (3), (4), (5)").unwrap();
        let r = run(&mut e, "SELECT id FROM t ORDER BY id LIMIT 2 OFFSET 2").unwrap();
        match r {
            ResultSet::Select { rows, .. } => {
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0][0], Value::Integer(3));
                assert_eq!(rows[1][0], Value::Integer(4));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn projection_with_alias_and_expression() {
        let mut e = MemoryEngine::new();
        run(&mut e, "CREATE TABLE t (a INT, b INT)").unwrap();
        run(&mut e, "INSERT INTO t VALUES (3, 4)").unwrap();
        let r = run(&mut e, "SELECT a + b AS sum, a * b AS prod FROM t").unwrap();
        match r {
            ResultSet::Select { columns, rows } => {
                assert_eq!(columns[0].name, "sum");
                assert_eq!(columns[1].name, "prod");
                assert_eq!(rows[0][0], Value::Integer(7));
                assert_eq!(rows[0][1], Value::Integer(12));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn update_with_where() {
        let mut e = MemoryEngine::new();
        run(&mut e, "CREATE TABLE t (id INT PRIMARY KEY, n INT)").unwrap();
        run(&mut e, "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)").unwrap();
        let r = run(&mut e, "UPDATE t SET n = n + 100 WHERE id >= 2").unwrap();
        match r {
            ResultSet::Update { count } => assert_eq!(count, 2),
            _ => panic!(),
        }
        let r = run(&mut e, "SELECT n FROM t ORDER BY id").unwrap();
        match r {
            ResultSet::Select { rows, .. } => {
                assert_eq!(rows[0][0], Value::Integer(10));
                assert_eq!(rows[1][0], Value::Integer(120));
                assert_eq!(rows[2][0], Value::Integer(130));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn delete_with_where() {
        let mut e = MemoryEngine::new();
        run(&mut e, "CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
        run(&mut e, "INSERT INTO t VALUES (1), (2), (3)").unwrap();
        let r = run(&mut e, "DELETE FROM t WHERE id = 2").unwrap();
        match r {
            ResultSet::Delete { count } => assert_eq!(count, 1),
            _ => panic!(),
        }
        let r = run(&mut e, "SELECT id FROM t ORDER BY id").unwrap();
        match r {
            ResultSet::Select { rows, .. } => assert_eq!(rows.len(), 2),
            _ => panic!(),
        }
    }

    #[test]
    fn drop_table_works() {
        let mut e = MemoryEngine::new();
        run(&mut e, "CREATE TABLE t (a INT)").unwrap();
        run(&mut e, "DROP TABLE t").unwrap();
        let r = run(&mut e, "SELECT * FROM t");
        assert!(r.is_err());
    }

    #[test]
    fn drop_if_exists_silent_when_missing() {
        let mut e = MemoryEngine::new();
        let r = run(&mut e, "DROP TABLE IF EXISTS missing").unwrap();
        match r {
            ResultSet::DropTable { existed, .. } => assert!(!existed),
            _ => panic!(),
        }
    }

    #[test]
    fn create_if_not_exists_silent_when_present() {
        let mut e = MemoryEngine::new();
        run(&mut e, "CREATE TABLE t (a INT)").unwrap();
        run(&mut e, "CREATE TABLE IF NOT EXISTS t (a INT)").unwrap();
    }

    #[test]
    fn varied_pipeline_via_run_all() {
        let mut e = MemoryEngine::new();
        let stmts = "
            CREATE TABLE k (id INT PRIMARY KEY, v STRING);
            INSERT INTO k VALUES (1, 'a'), (2, 'b'), (3, 'c');
            UPDATE k SET v = 'X' WHERE id = 2;
            DELETE FROM k WHERE id = 3;
        ";
        let _ = run_all(&mut e, stmts);
        let r = run(&mut e, "SELECT id, v FROM k ORDER BY id").unwrap();
        match r {
            ResultSet::Select { rows, .. } => {
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0][1], Value::String("a".into()));
                assert_eq!(rows[1][1], Value::String("X".into()));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn aggregate_without_group_by_rejected() {
        let mut e = MemoryEngine::new();
        run(&mut e, "CREATE TABLE t (a INT)").unwrap();
        let err = run(&mut e, "SELECT COUNT(*) FROM t").unwrap_err();
        assert!(err.to_string().contains("aggregate"));
    }
}
