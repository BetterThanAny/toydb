//! Statement executor.
//!
//! The executor is the bridge between the parsed [`Statement`] tree and
//! the [`Engine`] backend. SELECT runs through three pipelines depending
//! on shape: constant (no FROM), simple (single table, no aggregate),
//! and grouped (any aggregates or GROUP BY). Join handling is
//! orthogonal: the FROM clause materialises into a wide row set, and
//! either pipeline picks it up.

use std::cmp::Ordering;
use std::collections::HashMap;

use crate::catalog::Table;
use crate::engine::{Engine, RowId};
use crate::error::{Error, Result};
use crate::executor::aggregate::{
    aggregate_kind, collect_aggregates, eval_in_group, fold_row, fresh_accumulators, AggKey,
    AggSpec, Groups, GroupKey,
};
use crate::executor::expr::{eval, eval_with, Resolver};
use crate::executor::result::{Column as ResultColumn, ResultSet};
use crate::sql::ast::{
    CreateTableStmt, DeleteStmt, DropTableStmt, Expression, FromClause, InsertStmt, JoinKind,
    OrderBy, SelectItem, SelectStmt, Statement, UpdateStmt,
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
            for (idx, col) in table.columns.iter().enumerate() {
                if target_indices.contains(&idx) {
                    continue;
                }
                full[idx] = match &col.default {
                    Some(e) => eval(e)?,
                    None => Value::Null,
                };
            }
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
        // Constant query: no FROM.
        let Some(_) = s.from.as_ref() else {
            return self.exec_const_select(s);
        };

        // Materialise the FROM clause into a wide row set.
        let materialised = self.build_source(s.from.as_ref().unwrap())?;
        let WideSource { schema, mut rows } = materialised;

        // -- WHERE -----------------------------------------------------
        if let Some(predicate) = &s.r#where {
            let mut kept = Vec::with_capacity(rows.len());
            for row in rows {
                let resolver = WideResolver { schema: &schema, row: &row };
                match eval_with(predicate, &resolver)? {
                    Value::Boolean(true) => kept.push(row),
                    Value::Boolean(false) | Value::Null => {}
                    other => return Err(Error::ty(format!(
                        "WHERE expects boolean, got {}",
                        other.type_name()
                    ))),
                }
            }
            rows = kept;
        }

        // Detect aggregates anywhere in SELECT / HAVING.
        let mut aggs: Vec<(AggKey, AggSpec)> = Vec::new();
        for item in &s.items {
            if let SelectItem::Expr { expr, .. } = item {
                collect_aggregates(expr, &mut aggs)?;
            }
        }
        if let Some(having) = &s.having {
            collect_aggregates(having, &mut aggs)?;
        }

        let grouped = !s.group_by.is_empty() || !aggs.is_empty();

        if grouped {
            self.project_grouped(s, &schema, rows, aggs)
        } else {
            self.project_simple(s, &schema, rows)
        }
    }

    /// Constant SELECT (`SELECT 1+1`) — no FROM, single output row.
    fn exec_const_select(&mut self, s: &SelectStmt) -> Result<ResultSet> {
        if s.r#where.is_some() || !s.order_by.is_empty() || s.limit.is_some() {
            return Err(Error::other("WHERE/ORDER/LIMIT need a FROM clause"));
        }
        if !s.group_by.is_empty() || s.having.is_some() {
            return Err(Error::other("GROUP BY / HAVING need a FROM clause"));
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
                    let n = alias.clone().unwrap_or_else(|| format!("col{}", i + 1));
                    (n, v)
                }
            };
            columns.push(ResultColumn::new(name));
            row.push(value);
        }
        Ok(ResultSet::Select { columns, rows: vec![Row(row)] })
    }

    /// Simple (non-aggregated) projection.
    fn project_simple(
        &mut self,
        s: &SelectStmt,
        schema: &WideSchema,
        rows: Vec<Row>,
    ) -> Result<ResultSet> {
        // Build the projection plan once.
        let (columns, plan) = build_wide_projection(&s.items, schema)?;

        // Sort + LIMIT.
        let mut rows = rows;
        if !s.order_by.is_empty() {
            sort_rows_wide(&mut rows, &s.order_by, schema)?;
        }
        let offset = limit_eval(&s.offset, "OFFSET")?;
        let limit = limit_eval(&s.limit, "LIMIT")?;

        let mut out = Vec::new();
        for row in rows.into_iter().skip(offset).take(limit) {
            let resolver = WideResolver { schema, row: &row };
            let mut emitted = Vec::with_capacity(plan.len());
            for proj in &plan {
                let v = match proj {
                    WideProjection::Column(idx) => row.0[*idx].clone(),
                    WideProjection::Expression(e) => eval_with(e, &resolver)?,
                };
                emitted.push(v);
            }
            out.push(Row(emitted));
        }
        Ok(ResultSet::Select { columns, rows: out })
    }

    /// Grouped projection: builds groups, accumulates aggregates, then
    /// evaluates SELECT items per group with [`eval_in_group`].
    fn project_grouped(
        &mut self,
        s: &SelectStmt,
        schema: &WideSchema,
        rows: Vec<Row>,
        aggs: Vec<(AggKey, AggSpec)>,
    ) -> Result<ResultSet> {
        // Validate that non-aggregate SELECT items refer only to GROUP BY
        // expressions or constants.
        for item in &s.items {
            if let SelectItem::Expr { expr, .. } = item {
                let mut local = Vec::new();
                collect_aggregates(expr, &mut local)?;
                if local.is_empty() && !s.group_by.iter().any(|g| g == expr) && !is_constant(expr) {
                    return Err(Error::other(format!(
                        "non-aggregate item `{:?}` is not in GROUP BY",
                        expr
                    )));
                }
            }
        }

        // Group rows.
        let mut groups: Groups = Groups::new();
        // Preserve insertion order of group keys for deterministic output
        // when ORDER BY is omitted.
        let mut order: Vec<GroupKey> = Vec::new();
        // Implicit single-group when there's no GROUP BY but there are aggregates.
        if s.group_by.is_empty() {
            let key = GroupKey(vec![]);
            order.push(key.clone());
            let mut accs = fresh_accumulators(
                &aggs.iter().map(|(_, a)| a.clone()).collect::<Vec<_>>(),
            );
            // Capture an empty group representative for non-aggregate evaluation.
            // Allowed only when no non-aggregate SELECT items exist.
            for row in &rows {
                let resolver = WideResolver { schema, row };
                fold_row(
                    &mut accs,
                    &aggs.iter().map(|(_, a)| a.clone()).collect::<Vec<_>>(),
                    &resolver,
                )?;
            }
            groups.insert(key, accs);
        } else {
            let agg_specs: Vec<AggSpec> = aggs.iter().map(|(_, a)| a.clone()).collect();
            for row in &rows {
                let resolver = WideResolver { schema, row };
                let mut key = Vec::with_capacity(s.group_by.len());
                for g in &s.group_by {
                    key.push(eval_with(g, &resolver)?);
                }
                let key = GroupKey(key);
                let entry = groups.entry(key.clone()).or_insert_with(|| {
                    order.push(key.clone());
                    fresh_accumulators(&agg_specs)
                });
                fold_row(entry, &agg_specs, &resolver)?;
            }
        }

        // Build column descriptors and project per group.
        let columns: Vec<ResultColumn> = s
            .items
            .iter()
            .enumerate()
            .map(|(i, item)| match item {
                SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                    Err(Error::other("`*` in aggregated SELECT is unsupported"))
                }
                SelectItem::Expr { expr, alias } => {
                    let name = match (alias, expr) {
                        (Some(a), _) => a.clone(),
                        (None, Expression::Column(c)) => c.clone(),
                        (None, Expression::Qualified(_, c)) => c.clone(),
                        (None, Expression::Function { name, args })
                            if aggregate_kind(name).is_some() =>
                        {
                            if args.iter().any(|a| matches!(a, Expression::Wildcard)) {
                                format!("{}(*)", name.to_ascii_uppercase())
                            } else {
                                name.to_ascii_uppercase()
                            }
                        }
                        _ => format!("col{}", i + 1),
                    };
                    Ok(ResultColumn::new(name))
                }
            })
            .collect::<Result<_>>()?;

        // For each group, finalise accumulators and run projection / HAVING.
        struct Emitted { sort_keys: Vec<Value>, row: Row }
        let mut emitted: Vec<Emitted> = Vec::new();
        for key in &order {
            let accs = groups.get(key).expect("key from order list");
            let finals: Vec<Value> = accs.iter().map(|a| a.finalize()).collect();

            // Pick a representative row for resolving non-aggregate columns.
            // For non-empty groups we use the first matching row scanned.
            // For empty groups (no rows) we error if any non-aggregate
            // column is referenced — unless the group key fully covers it,
            // in which case the group key itself provides values.
            let rep_resolver = GroupedResolver {
                schema,
                rep_row: representative_row(&rows, &s.group_by, schema, key)?,
                key,
                group_by: &s.group_by,
            };

            // HAVING.
            if let Some(having) = &s.having {
                match eval_in_group(having, &aggs, &finals, &rep_resolver)? {
                    Value::Boolean(true) => {}
                    Value::Boolean(false) | Value::Null => continue,
                    other => return Err(Error::ty(format!(
                        "HAVING expects boolean, got {}",
                        other.type_name()
                    ))),
                }
            }

            // SELECT items.
            let mut row = Vec::with_capacity(s.items.len());
            for item in &s.items {
                let SelectItem::Expr { expr, .. } = item else {
                    return Err(Error::other("`*` in aggregated SELECT is unsupported"));
                };
                row.push(eval_in_group(expr, &aggs, &finals, &rep_resolver)?);
            }

            // ORDER BY keys. SELECT aliases (`AS total`) take priority
            // when they appear bare in ORDER BY — that matches Postgres
            // and SQLite. We substitute by walking the expression.
            let select_aliases: HashMap<&str, &Expression> = s
                .items
                .iter()
                .filter_map(|i| match i {
                    SelectItem::Expr { expr, alias: Some(a) } => Some((a.as_str(), expr)),
                    _ => None,
                })
                .collect();
            let mut sort_keys = Vec::new();
            for ob in &s.order_by {
                let rewritten = rewrite_aliases(&ob.expr, &select_aliases);
                sort_keys.push(eval_in_group(&rewritten, &aggs, &finals, &rep_resolver)?);
            }
            emitted.push(Emitted { sort_keys, row: Row(row) });
        }

        // ORDER BY.
        if !s.order_by.is_empty() {
            emitted.sort_by(|a, b| {
                for (i, ob) in s.order_by.iter().enumerate() {
                    let cmp = a.sort_keys[i].total_cmp(&b.sort_keys[i]);
                    if cmp != Ordering::Equal {
                        return if ob.asc { cmp } else { cmp.reverse() };
                    }
                }
                Ordering::Equal
            });
        }

        let offset = limit_eval(&s.offset, "OFFSET")?;
        let limit = limit_eval(&s.limit, "LIMIT")?;
        let rows: Vec<Row> = emitted
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|e| e.row)
            .collect();
        Ok(ResultSet::Select { columns, rows })
    }

    // ------------------------------------------------------------------
    // FROM materialisation (single table + nested-loop joins)
    // ------------------------------------------------------------------

    fn build_source(&mut self, from: &FromClause) -> Result<WideSource> {
        match from {
            FromClause::Table { name, alias } => {
                let table = self.engine.get_table(name)?.clone();
                let alias_str = alias.clone().unwrap_or_else(|| name.clone());
                let mut entries = Vec::new();
                for col in &table.columns {
                    entries.push(WideColumn {
                        alias: alias_str.clone(),
                        table_name: table.name.clone(),
                        column: col.name.clone(),
                    });
                }
                let raw = self.engine.scan(name)?;
                Ok(WideSource {
                    schema: WideSchema { columns: entries },
                    rows: raw.into_iter().map(|(_, r)| r).collect(),
                })
            }
            FromClause::Join { left, kind, right, on } => {
                let left = self.build_source(left)?;
                let right = self.build_source(right)?;
                let (schema, rows) = nested_loop_join(left, right, *kind, on)?;
                Ok(WideSource { schema, rows })
            }
        }
    }

    // ------------------------------------------------------------------
    // UPDATE
    // ------------------------------------------------------------------

    fn exec_update(&mut self, u: &UpdateStmt) -> Result<ResultSet> {
        let table = self.engine.get_table(&u.table)?.clone();
        let scan = self.engine.scan(&u.table)?;

        let mut assignments: Vec<(usize, &Expression)> = Vec::with_capacity(u.assignments.len());
        for (col, expr) in &u.assignments {
            let idx = table.column_index(col)?;
            assignments.push((idx, expr));
        }

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
// Wide-row schema and resolver
// ---------------------------------------------------------------------

#[derive(Debug, Clone)]
struct WideColumn {
    alias: String,
    table_name: String,
    column: String,
}

#[derive(Debug, Clone)]
pub(crate) struct WideSchema {
    columns: Vec<WideColumn>,
}

impl WideSchema {
    fn lookup(&self, name: &str) -> Result<usize> {
        let matches: Vec<_> = self
            .columns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.column == name)
            .collect();
        match matches.len() {
            0 => Err(Error::schema(format!("no such column `{name}`"))),
            1 => Ok(matches[0].0),
            _ => Err(Error::schema(format!(
                "ambiguous column `{name}` — qualify with table alias"
            ))),
        }
    }

    fn lookup_qualified(&self, alias: &str, name: &str) -> Result<usize> {
        for (i, c) in self.columns.iter().enumerate() {
            if (c.alias == alias || c.table_name == alias) && c.column == name {
                return Ok(i);
            }
        }
        Err(Error::schema(format!("no such column `{alias}.{name}`")))
    }

}

#[derive(Debug)]
struct WideSource {
    schema: WideSchema,
    rows: Vec<Row>,
}

struct WideResolver<'a> {
    schema: &'a WideSchema,
    row: &'a Row,
}

impl<'a> Resolver for WideResolver<'a> {
    fn column(&self, name: &str) -> Result<Value> {
        let i = self.schema.lookup(name)?;
        Ok(self.row.0[i].clone())
    }
    fn qualified(&self, table: &str, name: &str) -> Result<Value> {
        let i = self.schema.lookup_qualified(table, name)?;
        Ok(self.row.0[i].clone())
    }
}

/// Resolver used inside grouped projection. Falls back to a chosen
/// representative row for non-aggregate column references.
struct GroupedResolver<'a> {
    schema: &'a WideSchema,
    rep_row: Option<&'a Row>,
    key: &'a GroupKey,
    group_by: &'a [Expression],
}

impl<'a> Resolver for GroupedResolver<'a> {
    fn column(&self, name: &str) -> Result<Value> {
        // First try to find name in group-by expressions (matched as
        // `Expression::Column(name)`).
        for (i, g) in self.group_by.iter().enumerate() {
            if let Expression::Column(c) = g
                && c == name
            {
                return Ok(self.key.0[i].clone());
            }
        }
        match self.rep_row {
            Some(r) => {
                let i = self.schema.lookup(name)?;
                Ok(r.0[i].clone())
            }
            None => Err(Error::schema(format!(
                "column `{name}` is not in GROUP BY and no rows are available"
            ))),
        }
    }
    fn qualified(&self, table: &str, name: &str) -> Result<Value> {
        for (i, g) in self.group_by.iter().enumerate() {
            if let Expression::Qualified(t, c) = g
                && t == table
                && c == name
            {
                return Ok(self.key.0[i].clone());
            }
        }
        match self.rep_row {
            Some(r) => {
                let i = self.schema.lookup_qualified(table, name)?;
                Ok(r.0[i].clone())
            }
            None => Err(Error::schema(format!(
                "column `{table}.{name}` is not in GROUP BY and no rows are available"
            ))),
        }
    }
}

fn representative_row<'a>(
    rows: &'a [Row],
    group_by: &[Expression],
    schema: &WideSchema,
    key: &GroupKey,
) -> Result<Option<&'a Row>> {
    if group_by.is_empty() {
        return Ok(rows.first());
    }
    for r in rows {
        let resolver = WideResolver { schema, row: r };
        let mut k = Vec::with_capacity(group_by.len());
        for g in group_by {
            k.push(eval_with(g, &resolver)?);
        }
        if GroupKey(k) == *key {
            return Ok(Some(r));
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------
// SingleTable resolver — kept for UPDATE/DELETE which only ever see one
// table at a time.
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
// Joins
// ---------------------------------------------------------------------

fn nested_loop_join(
    left: WideSource,
    right: WideSource,
    kind: JoinKind,
    on: &Expression,
) -> Result<(WideSchema, Vec<Row>)> {
    let mut combined_columns = left.schema.columns.clone();
    combined_columns.extend(right.schema.columns.iter().cloned());
    let combined_schema = WideSchema { columns: combined_columns };
    let right_arity = right.schema.columns.len();
    let left_arity = left.schema.columns.len();

    let mut output: Vec<Row> = Vec::new();
    match kind {
        JoinKind::Inner | JoinKind::Left | JoinKind::Right => {}
    }

    let outer_first = matches!(kind, JoinKind::Left | JoinKind::Inner);
    let (outer_rows, inner_rows, outer_schema, inner_schema, outer_arity, inner_arity, swap) =
        if outer_first {
            (
                &left.rows,
                &right.rows,
                &left.schema,
                &right.schema,
                left_arity,
                right_arity,
                false,
            )
        } else {
            (
                &right.rows,
                &left.rows,
                &right.schema,
                &left.schema,
                right_arity,
                left_arity,
                true,
            )
        };

    let preserve_outer = matches!(kind, JoinKind::Left | JoinKind::Right);

    for outer in outer_rows {
        let mut matched = false;
        for inner in inner_rows {
            // Build candidate row with original (left-then-right) order.
            let mut wide = Vec::with_capacity(outer_arity + inner_arity);
            if swap {
                wide.extend_from_slice(&inner.0);
                wide.extend_from_slice(&outer.0);
            } else {
                wide.extend_from_slice(&outer.0);
                wide.extend_from_slice(&inner.0);
            }
            let row = Row(wide);
            let resolver = WideResolver { schema: &combined_schema, row: &row };
            let truthy = match eval_with(on, &resolver)? {
                Value::Boolean(b) => b,
                Value::Null => false,
                other => return Err(Error::ty(format!(
                    "ON expects boolean, got {}",
                    other.type_name()
                ))),
            };
            if truthy {
                matched = true;
                output.push(row);
            }
        }
        if !matched && preserve_outer {
            // Pad inner with NULLs.
            let mut wide = Vec::with_capacity(outer_arity + inner_arity);
            if swap {
                for _ in 0..inner_arity { wide.push(Value::Null); }
                wide.extend_from_slice(&outer.0);
            } else {
                wide.extend_from_slice(&outer.0);
                for _ in 0..inner_arity { wide.push(Value::Null); }
            }
            output.push(Row(wide));
        }
    }
    let _ = outer_schema; let _ = inner_schema;
    Ok((combined_schema, output))
}

// ---------------------------------------------------------------------
// Projection plan (wide variant)
// ---------------------------------------------------------------------

enum WideProjection<'a> {
    Column(usize),
    Expression(&'a Expression),
}

fn build_wide_projection<'a>(
    items: &'a [SelectItem],
    schema: &WideSchema,
) -> Result<(Vec<ResultColumn>, Vec<WideProjection<'a>>)> {
    let mut columns = Vec::new();
    let mut plan = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard => {
                for (i, c) in schema.columns.iter().enumerate() {
                    columns.push(ResultColumn::new(c.column.clone()));
                    plan.push(WideProjection::Column(i));
                }
            }
            SelectItem::QualifiedWildcard(alias) => {
                let mut any = false;
                for (i, c) in schema.columns.iter().enumerate() {
                    if c.alias == *alias || c.table_name == *alias {
                        columns.push(ResultColumn::new(c.column.clone()));
                        plan.push(WideProjection::Column(i));
                        any = true;
                    }
                }
                if !any {
                    return Err(Error::schema(format!(
                        "qualified wildcard `{alias}.*` matches no source"
                    )));
                }
            }
            SelectItem::Expr { expr, alias } => {
                let display = match (alias, expr) {
                    (Some(a), _) => a.clone(),
                    (None, Expression::Column(c)) => c.clone(),
                    (None, Expression::Qualified(_, c)) => c.clone(),
                    (None, _) => "expr".to_string(),
                };
                columns.push(ResultColumn::new(display));
                plan.push(WideProjection::Expression(expr));
            }
        }
    }
    if columns.is_empty() {
        return Err(Error::schema("SELECT requires at least one item"));
    }
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

fn sort_rows_wide(rows: &mut [Row], order_by: &[OrderBy], schema: &WideSchema) -> Result<()> {
    struct Keyed { keys: Vec<Value>, row: Row }
    let mut keyed: Vec<Keyed> = Vec::with_capacity(rows.len());
    for row in rows.iter() {
        let resolver = WideResolver { schema, row };
        let mut k = Vec::with_capacity(order_by.len());
        for ob in order_by {
            k.push(eval_with(&ob.expr, &resolver)?);
        }
        keyed.push(Keyed { keys: k, row: row.clone() });
    }
    keyed.sort_by(|a, b| {
        for (i, ob) in order_by.iter().enumerate() {
            let cmp = a.keys[i].total_cmp(&b.keys[i]);
            if cmp != Ordering::Equal {
                return if ob.asc { cmp } else { cmp.reverse() };
            }
        }
        Ordering::Equal
    });
    for (i, k) in keyed.into_iter().enumerate() {
        rows[i] = k.row;
    }
    Ok(())
}

fn limit_eval(expr: &Option<Expression>, label: &str) -> Result<usize> {
    match expr {
        None => Ok(if label == "LIMIT" { usize::MAX } else { 0 }),
        Some(e) => {
            let v = eval(e)?;
            match v {
                Value::Integer(n) if n >= 0 => Ok(n as usize),
                Value::Integer(n) => Err(Error::value(format!("{label} must be non-negative, got {n}"))),
                Value::Null => Ok(0),
                other => Err(Error::ty(format!("{label} expects integer, got {}", other.type_name()))),
            }
        }
    }
}

fn is_constant(e: &Expression) -> bool {
    match e {
        Expression::Literal(_) => true,
        Expression::Unary(_, inner) => is_constant(inner),
        Expression::Binary(l, _, r) => is_constant(l) && is_constant(r),
        _ => false,
    }
}

/// Substitute bare `Column(name)` references with the SELECT-clause
/// expression aliased to that name. Used so that `ORDER BY total` can
/// refer to `... AS total` in the same query.
fn rewrite_aliases(expr: &Expression, aliases: &HashMap<&str, &Expression>) -> Expression {
    match expr {
        Expression::Column(name) => {
            if let Some(replacement) = aliases.get(name.as_str()) {
                (*replacement).clone()
            } else {
                expr.clone()
            }
        }
        Expression::Qualified(_, _) => expr.clone(),
        Expression::Literal(_) | Expression::Wildcard => expr.clone(),
        Expression::Unary(op, inner) => {
            Expression::Unary(*op, Box::new(rewrite_aliases(inner, aliases)))
        }
        Expression::Binary(l, op, r) => Expression::Binary(
            Box::new(rewrite_aliases(l, aliases)),
            *op,
            Box::new(rewrite_aliases(r, aliases)),
        ),
        Expression::IsNull { expr, negated } => Expression::IsNull {
            expr: Box::new(rewrite_aliases(expr, aliases)),
            negated: *negated,
        },
        Expression::InList { expr, list, negated } => Expression::InList {
            expr: Box::new(rewrite_aliases(expr, aliases)),
            list: list.iter().map(|e| rewrite_aliases(e, aliases)).collect(),
            negated: *negated,
        },
        Expression::Between { expr, low, high, negated } => Expression::Between {
            expr: Box::new(rewrite_aliases(expr, aliases)),
            low: Box::new(rewrite_aliases(low, aliases)),
            high: Box::new(rewrite_aliases(high, aliases)),
            negated: *negated,
        },
        Expression::Like { expr, pattern, negated } => Expression::Like {
            expr: Box::new(rewrite_aliases(expr, aliases)),
            pattern: Box::new(rewrite_aliases(pattern, aliases)),
            negated: *negated,
        },
        Expression::Function { name, args } => Expression::Function {
            name: name.clone(),
            args: args.iter().map(|a| rewrite_aliases(a, aliases)).collect(),
        },
    }
}

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

    fn assert_select(rs: &ResultSet) -> &[Row] {
        match rs {
            ResultSet::Select { rows, .. } => rows,
            other => panic!("expected Select, got {other:?}"),
        }
    }

    // ----- existing M5 tests: still must pass --------------------------

    #[test]
    fn create_insert_select_basic() {
        let mut e = MemoryEngine::new();
        run(&mut e, "CREATE TABLE t (id INT PRIMARY KEY, n STRING)").unwrap();
        run(&mut e, "INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')").unwrap();
        let r = run(&mut e, "SELECT * FROM t").unwrap();
        assert_eq!(assert_select(&r).len(), 3);
    }

    #[test]
    fn select_const() {
        let mut e = MemoryEngine::new();
        let r = run(&mut e, "SELECT 1 + 1").unwrap();
        assert_eq!(assert_select(&r)[0][0], Value::Integer(2));
    }

    #[test]
    fn where_filter_simple() {
        let mut e = MemoryEngine::new();
        run(&mut e, "CREATE TABLE t (id INT PRIMARY KEY, age INT)").unwrap();
        run(&mut e, "INSERT INTO t VALUES (1, 18), (2, 25), (3, 12)").unwrap();
        let r = run(&mut e, "SELECT id FROM t WHERE age >= 18").unwrap();
        assert_eq!(assert_select(&r).len(), 2);
    }

    #[test]
    fn order_limit_offset() {
        let mut e = MemoryEngine::new();
        run(&mut e, "CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
        run(&mut e, "INSERT INTO t VALUES (5), (3), (1), (4), (2)").unwrap();
        let r = run(&mut e, "SELECT id FROM t ORDER BY id LIMIT 2 OFFSET 2").unwrap();
        let rows = assert_select(&r);
        assert_eq!(rows[0][0], Value::Integer(3));
        assert_eq!(rows[1][0], Value::Integer(4));
    }

    #[test]
    fn update_with_where() {
        let mut e = MemoryEngine::new();
        run(&mut e, "CREATE TABLE t (id INT PRIMARY KEY, n INT)").unwrap();
        run(&mut e, "INSERT INTO t VALUES (1, 10), (2, 20)").unwrap();
        run(&mut e, "UPDATE t SET n = n + 100 WHERE id = 2").unwrap();
        let r = run(&mut e, "SELECT n FROM t ORDER BY id").unwrap();
        let rows = assert_select(&r);
        assert_eq!(rows[0][0], Value::Integer(10));
        assert_eq!(rows[1][0], Value::Integer(120));
    }

    // ----- aggregates --------------------------------------------------

    fn setup_orders(e: &mut MemoryEngine) {
        run_all(e, "
            CREATE TABLE orders (id INT PRIMARY KEY, customer TEXT, amount INT, status TEXT);
            INSERT INTO orders VALUES
                (1, 'alice', 100, 'paid'),
                (2, 'bob',   200, 'paid'),
                (3, 'alice', 150, 'paid'),
                (4, 'carol',  50, 'pending'),
                (5, 'bob',   300, 'paid'),
                (6, 'alice', NULL, 'paid');
        ");
    }

    #[test]
    fn count_star_no_group() {
        let mut e = MemoryEngine::new();
        setup_orders(&mut e);
        let r = run(&mut e, "SELECT COUNT(*) FROM orders").unwrap();
        let rows = assert_select(&r);
        assert_eq!(rows[0][0], Value::Integer(6));
    }

    #[test]
    fn sum_skips_nulls() {
        let mut e = MemoryEngine::new();
        setup_orders(&mut e);
        let r = run(&mut e, "SELECT SUM(amount) FROM orders WHERE status = 'paid'").unwrap();
        let rows = assert_select(&r);
        // 100 + 200 + 150 + 300 (excludes NULL row 6 and pending row 4)
        assert_eq!(rows[0][0], Value::Integer(750));
    }

    #[test]
    fn count_excludes_null_argument() {
        let mut e = MemoryEngine::new();
        setup_orders(&mut e);
        let r = run(&mut e, "SELECT COUNT(amount), COUNT(*) FROM orders").unwrap();
        let rows = assert_select(&r);
        assert_eq!(rows[0][0], Value::Integer(5));
        assert_eq!(rows[0][1], Value::Integer(6));
    }

    #[test]
    fn group_by_customer() {
        let mut e = MemoryEngine::new();
        setup_orders(&mut e);
        let r = run(
            &mut e,
            "SELECT customer, COUNT(*), SUM(amount) FROM orders GROUP BY customer ORDER BY customer",
        )
        .unwrap();
        let rows = assert_select(&r);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][0], Value::String("alice".into()));
        assert_eq!(rows[0][1], Value::Integer(3));
        assert_eq!(rows[0][2], Value::Integer(250));
        assert_eq!(rows[1][0], Value::String("bob".into()));
        assert_eq!(rows[1][1], Value::Integer(2));
        assert_eq!(rows[1][2], Value::Integer(500));
    }

    #[test]
    fn having_filters_groups() {
        let mut e = MemoryEngine::new();
        setup_orders(&mut e);
        let r = run(
            &mut e,
            "SELECT customer, COUNT(*) FROM orders GROUP BY customer HAVING COUNT(*) >= 2 ORDER BY customer",
        )
        .unwrap();
        let rows = assert_select(&r);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn avg_and_min_max() {
        let mut e = MemoryEngine::new();
        setup_orders(&mut e);
        let r = run(
            &mut e,
            "SELECT customer, AVG(amount), MIN(amount), MAX(amount) FROM orders WHERE status = 'paid' GROUP BY customer ORDER BY customer",
        )
        .unwrap();
        let rows = assert_select(&r);
        match &rows[0][1] {
            Value::Float(_) => {}
            other => panic!("AVG should be float, got {other:?}"),
        }
        assert_eq!(rows[0][2], Value::Integer(100)); // alice min
        assert_eq!(rows[0][3], Value::Integer(150)); // alice max
    }

    #[test]
    fn non_aggregate_outside_group_by_rejected() {
        let mut e = MemoryEngine::new();
        setup_orders(&mut e);
        let err = run(
            &mut e,
            "SELECT customer, status, COUNT(*) FROM orders GROUP BY customer",
        )
        .unwrap_err();
        assert!(err.to_string().contains("not in GROUP BY"));
    }

    // ----- joins -------------------------------------------------------

    fn setup_join(e: &mut MemoryEngine) {
        run_all(e, "
            CREATE TABLE users (id INT PRIMARY KEY, name TEXT);
            CREATE TABLE posts (id INT PRIMARY KEY, user_id INT, title TEXT);
            INSERT INTO users VALUES (1, 'alice'), (2, 'bob'), (3, 'carol');
            INSERT INTO posts VALUES
                (1, 1, 'hello'),
                (2, 1, 'world'),
                (3, 2, 'rust');
        ");
    }

    #[test]
    fn inner_join_basic() {
        let mut e = MemoryEngine::new();
        setup_join(&mut e);
        let r = run(
            &mut e,
            "SELECT u.name, p.title FROM users u INNER JOIN posts p ON u.id = p.user_id ORDER BY p.id",
        )
        .unwrap();
        let rows = assert_select(&r);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][0], Value::String("alice".into()));
        assert_eq!(rows[2][0], Value::String("bob".into()));
    }

    #[test]
    fn left_join_includes_orphans() {
        let mut e = MemoryEngine::new();
        setup_join(&mut e);
        let r = run(
            &mut e,
            "SELECT u.name, p.title FROM users u LEFT JOIN posts p ON u.id = p.user_id ORDER BY u.id, p.id",
        )
        .unwrap();
        let rows = assert_select(&r);
        // alice has 2 posts, bob has 1, carol has 0 → 4 total
        assert_eq!(rows.len(), 4);
        // Carol's row has NULL title.
        let carol_rows: Vec<_> = rows.iter().filter(|r| r[0] == Value::String("carol".into())).collect();
        assert_eq!(carol_rows.len(), 1);
        assert_eq!(carol_rows[0][1], Value::Null);
    }

    #[test]
    fn join_combined_with_where() {
        let mut e = MemoryEngine::new();
        setup_join(&mut e);
        let r = run(
            &mut e,
            "SELECT u.name FROM users u INNER JOIN posts p ON u.id = p.user_id WHERE p.title = 'rust'",
        )
        .unwrap();
        let rows = assert_select(&r);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], Value::String("bob".into()));
    }

    #[test]
    fn join_with_aggregate() {
        let mut e = MemoryEngine::new();
        setup_join(&mut e);
        let r = run(
            &mut e,
            "SELECT u.name, COUNT(*) FROM users u INNER JOIN posts p ON u.id = p.user_id GROUP BY u.name ORDER BY u.name",
        )
        .unwrap();
        let rows = assert_select(&r);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][1], Value::Integer(2));
        assert_eq!(rows[1][1], Value::Integer(1));
    }
}
