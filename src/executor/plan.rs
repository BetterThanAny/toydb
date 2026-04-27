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
    AlterAction, AlterTableStmt, CreateTableStmt, DeleteStmt, DropTableStmt, Expression,
    FromClause, InsertSource, InsertStmt, JoinKind, OrderBy, SelectItem, SelectStmt, Statement,
    UpdateStmt,
};
use crate::types::row::Row;
use crate::types::value::Value;

pub struct Executor<'a> {
    engine: &'a mut dyn Engine,
}

impl<'a> Executor<'a> {
    pub fn new(engine: &'a mut dyn Engine) -> Self { Self { engine } }

    /// Run one statement against the engine.
    ///
    /// Before dispatch we resolve any scalar subqueries in the statement
    /// (uncorrelated only) by running them and folding the result down
    /// to a literal. This keeps the rest of the pipeline subquery-free.
    pub fn execute(&mut self, stmt: &Statement) -> Result<ResultSet> {
        let mut stmt = stmt.clone();
        self.resolve_subqueries_stmt(&mut stmt)?;
        match &stmt {
            Statement::CreateTable(s) => self.exec_create_table(s),
            Statement::DropTable(s) => self.exec_drop_table(s),
            Statement::AlterTable(s) => self.exec_alter_table(s),
            Statement::Insert(s) => self.exec_insert(s),
            Statement::Select(s) => self.exec_select(s),
            Statement::Update(s) => self.exec_update(s),
            Statement::Delete(s) => self.exec_delete(s),
            Statement::Begin => {
                self.engine.begin()?;
                Ok(ResultSet::Begin)
            }
            Statement::Commit => {
                self.engine.commit()?;
                Ok(ResultSet::Commit)
            }
            Statement::Rollback => {
                self.engine.rollback()?;
                Ok(ResultSet::Rollback)
            }
            Statement::Explain(inner) => Ok(ResultSet::Explain(describe_plan(inner))),
        }
    }

    // ------------------------------------------------------------------
    // Scalar subquery resolution
    // ------------------------------------------------------------------

    fn resolve_subqueries_stmt(&mut self, stmt: &mut Statement) -> Result<()> {
        match stmt {
            Statement::Select(s) => self.resolve_subqueries_select(s),
            Statement::Insert(i) => {
                if let InsertSource::Values(rows) = &mut i.source {
                    for row in rows {
                        for e in row.iter_mut() {
                            self.resolve_subqueries_expr(e)?;
                        }
                    }
                }
                if let InsertSource::Select(inner) = &mut i.source {
                    self.resolve_subqueries_select(inner)?;
                }
                Ok(())
            }
            Statement::Update(u) => {
                for (_, e) in u.assignments.iter_mut() {
                    self.resolve_subqueries_expr(e)?;
                }
                if let Some(w) = u.r#where.as_mut() {
                    self.resolve_subqueries_expr(w)?;
                }
                Ok(())
            }
            Statement::Delete(d) => {
                if let Some(w) = d.r#where.as_mut() {
                    self.resolve_subqueries_expr(w)?;
                }
                Ok(())
            }
            Statement::Explain(inner) => self.resolve_subqueries_stmt(inner),
            Statement::CreateTable(_)
            | Statement::DropTable(_)
            | Statement::AlterTable(_)
            | Statement::Begin
            | Statement::Commit
            | Statement::Rollback => Ok(()),
        }
    }

    fn resolve_subqueries_select(&mut self, s: &mut SelectStmt) -> Result<()> {
        for item in &mut s.items {
            if let SelectItem::Expr { expr, .. } = item {
                self.resolve_subqueries_expr(expr)?;
            }
        }
        if let Some(w) = s.r#where.as_mut() {
            self.resolve_subqueries_expr(w)?;
        }
        for g in &mut s.group_by {
            self.resolve_subqueries_expr(g)?;
        }
        if let Some(h) = s.having.as_mut() {
            self.resolve_subqueries_expr(h)?;
        }
        for ob in &mut s.order_by {
            self.resolve_subqueries_expr(&mut ob.expr)?;
        }
        if let Some(l) = s.limit.as_mut() {
            self.resolve_subqueries_expr(l)?;
        }
        if let Some(o) = s.offset.as_mut() {
            self.resolve_subqueries_expr(o)?;
        }
        for u in &mut s.unions {
            self.resolve_subqueries_select(&mut u.query)?;
        }
        Ok(())
    }

    fn resolve_subqueries_expr(&mut self, e: &mut Expression) -> Result<()> {
        // Replace Scalar(stmt) with a literal value, recurse into kids.
        if let Expression::Scalar(inner) = e {
            // Recursively resolve nested subqueries.
            self.resolve_subqueries_select(inner)?;
            let value = self.run_scalar_subquery(inner)?;
            *e = Expression::Literal(literal_for_value(value));
            return Ok(());
        }
        match e {
            Expression::Literal(_)
            | Expression::Column(_)
            | Expression::Qualified(_, _)
            | Expression::Wildcard => {}
            Expression::Unary(_, inner) => self.resolve_subqueries_expr(inner)?,
            Expression::Binary(l, _, r) => {
                self.resolve_subqueries_expr(l)?;
                self.resolve_subqueries_expr(r)?;
            }
            Expression::IsNull { expr, .. } => self.resolve_subqueries_expr(expr)?,
            Expression::InList { expr, list, .. } => {
                self.resolve_subqueries_expr(expr)?;
                for it in list {
                    self.resolve_subqueries_expr(it)?;
                }
            }
            Expression::Between { expr, low, high, .. } => {
                self.resolve_subqueries_expr(expr)?;
                self.resolve_subqueries_expr(low)?;
                self.resolve_subqueries_expr(high)?;
            }
            Expression::Like { expr, pattern, .. } => {
                self.resolve_subqueries_expr(expr)?;
                self.resolve_subqueries_expr(pattern)?;
            }
            Expression::Function { args, .. } => {
                for a in args {
                    self.resolve_subqueries_expr(a)?;
                }
            }
            Expression::Case { operand, branches, otherwise } => {
                if let Some(op) = operand.as_mut() {
                    self.resolve_subqueries_expr(op)?;
                }
                for (w, t) in branches {
                    self.resolve_subqueries_expr(w)?;
                    self.resolve_subqueries_expr(t)?;
                }
                if let Some(o) = otherwise.as_mut() {
                    self.resolve_subqueries_expr(o)?;
                }
            }
            Expression::Scalar(_) => unreachable!("handled above"),
        }
        Ok(())
    }

    fn run_scalar_subquery(&mut self, s: &SelectStmt) -> Result<Value> {
        let rs = self.exec_select(s)?;
        match rs {
            ResultSet::Select { rows, columns } => {
                if columns.len() != 1 {
                    return Err(Error::other(format!(
                        "scalar subquery returned {} columns, expected 1",
                        columns.len()
                    )));
                }
                match rows.len() {
                    0 => Ok(Value::Null),
                    1 => Ok(rows.into_iter().next().unwrap().0.into_iter().next().unwrap()),
                    n => Err(Error::other(format!(
                        "scalar subquery returned {n} rows, expected 0 or 1"
                    ))),
                }
            }
            _ => Err(Error::internal("inner SELECT did not return rows")),
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

    fn exec_alter_table(&mut self, a: &AlterTableStmt) -> Result<ResultSet> {
        match &a.action {
            AlterAction::AddColumn(def) => {
                // Validate constant default at create time.
                if let Some(d) = &def.default {
                    eval(d).map_err(|e| {
                        Error::schema(format!(
                            "DEFAULT for column `{}` is not constant: {e}",
                            def.name
                        ))
                    })?;
                }
                let col: crate::catalog::Column = def.into();
                self.engine.add_column(&a.name, col)?;
                Ok(ResultSet::AlterTable { name: a.name.clone() })
            }
        }
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

        // Materialise the rows to insert. For VALUES we just collect the
        // ASTs; for SELECT we run the inner query and use its result set.
        let raw_rows: Vec<Row> = match &i.source {
            InsertSource::Values(rows) => {
                let mut out = Vec::with_capacity(rows.len());
                for raw in rows {
                    if raw.len() != target_indices.len() {
                        return Err(Error::ty(format!(
                            "INSERT into {}: expected {} values, got {}",
                            i.table,
                            target_indices.len(),
                            raw.len()
                        )));
                    }
                    let mut row = Vec::with_capacity(raw.len());
                    for e in raw {
                        row.push(eval(e)?);
                    }
                    out.push(Row(row));
                }
                out
            }
            InsertSource::Select(inner) => {
                let rs = self.exec_select(inner)?;
                match rs {
                    ResultSet::Select { columns, rows } => {
                        if columns.len() != target_indices.len() {
                            return Err(Error::ty(format!(
                                "INSERT INTO {}: SELECT produces {} columns, expected {}",
                                i.table,
                                columns.len(),
                                target_indices.len()
                            )));
                        }
                        rows
                    }
                    _ => return Err(Error::internal("inner SELECT did not return rows")),
                }
            }
        };

        let mut count = 0usize;
        for incoming in raw_rows {
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
            for (slot, value) in target_indices.iter().zip(incoming.0) {
                full[*slot] = value;
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
        if !s.unions.is_empty() {
            return self.exec_union(s);
        }
        self.exec_select_inner(s)
    }

    fn exec_union(&mut self, s: &SelectStmt) -> Result<ResultSet> {
        // We don't yet support sort/limit/offset on top of a UNION
        // because they'd need a column-name resolver over the combined
        // result set. Reject up front so the user gets a clear error.
        if !s.order_by.is_empty() || s.limit.is_some() || s.offset.is_some() {
            return Err(Error::other(
                "ORDER BY / LIMIT / OFFSET on a UNION'ed result is not supported yet",
            ));
        }
        // Run the head as if it had no unions.
        let mut head = s.clone();
        head.unions.clear();
        let head_rs = self.exec_select_inner(&head)?;
        let (mut columns, mut rows) = match head_rs {
            ResultSet::Select { columns, rows } => (columns, rows),
            _ => return Err(Error::internal("UNION head did not return Select")),
        };
        // `dedupe` becomes true the moment we encounter any non-ALL UNION.
        let mut dedupe = false;
        for u in &s.unions {
            let mut sub = (*u.query).clone();
            sub.unions.clear();
            sub.order_by.clear();
            sub.limit = None;
            sub.offset = None;
            let rs = self.exec_select_inner(&sub)?;
            let (sub_cols, sub_rows) = match rs {
                ResultSet::Select { columns, rows } => (columns, rows),
                _ => return Err(Error::internal("UNION arm did not return Select")),
            };
            if sub_cols.len() != columns.len() {
                return Err(Error::other(format!(
                    "UNION column count mismatch ({} vs {})",
                    columns.len(),
                    sub_cols.len()
                )));
            }
            rows.extend(sub_rows);
            if !u.all {
                dedupe = true;
            }
        }
        if dedupe {
            let mut seen: std::collections::BTreeSet<GroupKey> = std::collections::BTreeSet::new();
            let mut out = Vec::with_capacity(rows.len());
            for r in rows {
                let key = GroupKey(r.0.clone());
                if seen.insert(key) {
                    out.push(r);
                }
            }
            rows = out;
        }
        let _ = &mut columns;
        Ok(ResultSet::Select { columns, rows })
    }

    fn exec_select_inner(&mut self, s: &SelectStmt) -> Result<ResultSet> {
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

        // Sort + LIMIT. ORDER BY may reference SELECT aliases (Postgres
        // semantics) — rewrite those before sorting.
        let select_aliases: HashMap<&str, &Expression> = s
            .items
            .iter()
            .filter_map(|i| match i {
                SelectItem::Expr { expr, alias: Some(a) } => Some((a.as_str(), expr)),
                _ => None,
            })
            .collect();
        let order_by: Vec<OrderBy> = s
            .order_by
            .iter()
            .map(|ob| OrderBy {
                expr: rewrite_aliases(&ob.expr, &select_aliases),
                asc: ob.asc,
                nulls_first: ob.nulls_first,
            })
            .collect();
        let mut rows = rows;
        if !order_by.is_empty() {
            sort_rows_wide(&mut rows, &order_by, schema)?;
        }
        let offset = limit_eval(&s.offset, "OFFSET")?;
        let limit = limit_eval(&s.limit, "LIMIT")?;

        // Project each row; if DISTINCT is set, dedupe by total ordering.
        let mut projected: Vec<Row> = Vec::new();
        let mut seen: std::collections::BTreeSet<GroupKey> = std::collections::BTreeSet::new();
        for row in rows.into_iter().skip(offset) {
            if projected.len() >= limit { break; }
            let resolver = WideResolver { schema, row: &row };
            let mut emitted = Vec::with_capacity(plan.len());
            for proj in &plan {
                let v = match proj {
                    WideProjection::Column(idx) => row.0[*idx].clone(),
                    WideProjection::Expression(e) => eval_with(e, &resolver)?,
                };
                emitted.push(v);
            }
            if s.distinct {
                let key = GroupKey(emitted.clone());
                if !seen.insert(key) {
                    continue;
                }
            }
            projected.push(Row(emitted));
        }
        Ok(ResultSet::Select { columns, rows: projected })
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
                        (None, Expression::Function { name, args, distinct })
                            if aggregate_kind(name).is_some() =>
                        {
                            let upper = name.to_ascii_uppercase();
                            if args.iter().any(|a| matches!(a, Expression::Wildcard)) {
                                format!("{}(*)", upper)
                            } else if *distinct {
                                format!("{}(DISTINCT)", upper)
                            } else {
                                upper
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
                    let av = &a.sort_keys[i];
                    let bv = &b.sort_keys[i];
                    match (av.is_null(), bv.is_null()) {
                        (true, true) => continue,
                        (true, false) => {
                            return if ob.nulls_first { Ordering::Less } else { Ordering::Greater };
                        }
                        (false, true) => {
                            return if ob.nulls_first { Ordering::Greater } else { Ordering::Less };
                        }
                        (false, false) => {}
                    }
                    let cmp = av.total_cmp(bv);
                    if cmp != Ordering::Equal {
                        return if ob.asc { cmp } else { cmp.reverse() };
                    }
                }
                Ordering::Equal
            });
        }

        let offset = limit_eval(&s.offset, "OFFSET")?;
        let limit = limit_eval(&s.limit, "LIMIT")?;
        let mut seen: std::collections::BTreeSet<GroupKey> = std::collections::BTreeSet::new();
        let mut rows: Vec<Row> = Vec::new();
        for e in emitted.into_iter().skip(offset) {
            if rows.len() >= limit { break; }
            if s.distinct {
                let key = GroupKey(e.row.0.clone());
                if !seen.insert(key) { continue; }
            }
            rows.push(e.row);
        }
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
            let av = &a.keys[i];
            let bv = &b.keys[i];
            // Apply NULLS FIRST/LAST first.
            match (av.is_null(), bv.is_null()) {
                (true, true) => continue,
                (true, false) => return if ob.nulls_first { Ordering::Less } else { Ordering::Greater },
                (false, true) => return if ob.nulls_first { Ordering::Greater } else { Ordering::Less },
                (false, false) => {}
            }
            let cmp = av.total_cmp(bv);
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

fn literal_for_value(v: Value) -> crate::sql::ast::Literal {
    use crate::sql::ast::Literal;
    match v {
        Value::Null => Literal::Null,
        Value::Boolean(b) => Literal::Boolean(b),
        Value::Integer(n) => Literal::Integer(n),
        Value::Float(f) => Literal::Float(f),
        Value::String(s) => Literal::String(s),
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
        Expression::Literal(_) | Expression::Wildcard | Expression::Scalar(_) => expr.clone(),
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
        Expression::Function { name, args, distinct } => Expression::Function {
            name: name.clone(),
            args: args.iter().map(|a| rewrite_aliases(a, aliases)).collect(),
            distinct: *distinct,
        },
        Expression::Case { operand, branches, otherwise } => Expression::Case {
            operand: operand.as_ref().map(|e| Box::new(rewrite_aliases(e, aliases))),
            branches: branches
                .iter()
                .map(|(w, t)| (rewrite_aliases(w, aliases), rewrite_aliases(t, aliases)))
                .collect(),
            otherwise: otherwise.as_ref().map(|e| Box::new(rewrite_aliases(e, aliases))),
        },
    }
}

// ---------------------------------------------------------------------
// EXPLAIN — textual plan
// ---------------------------------------------------------------------

fn describe_plan(stmt: &Statement) -> String {
    match stmt {
        Statement::Select(s) => describe_select(s),
        Statement::Insert(i) => match &i.source {
            InsertSource::Values(rows) => {
                format!("Insert into `{}` ({} rows from VALUES)", i.table, rows.len())
            }
            InsertSource::Select(_) => format!("Insert into `{}` (from SELECT)", i.table),
        },
        Statement::Update(u) => format!(
            "Update `{}`{}",
            u.table,
            if u.r#where.is_some() { " (filtered)" } else { "" }
        ),
        Statement::Delete(d) => format!(
            "Delete from `{}`{}",
            d.table,
            if d.r#where.is_some() { " (filtered)" } else { "" }
        ),
        Statement::CreateTable(c) => format!("CreateTable `{}`", c.name),
        Statement::DropTable(d) => format!("DropTable `{}`", d.name),
        Statement::AlterTable(a) => format!("AlterTable `{}`", a.name),
        Statement::Begin => "Begin".into(),
        Statement::Commit => "Commit".into(),
        Statement::Rollback => "Rollback".into(),
        Statement::Explain(inner) => format!("Explain (nested):\n{}", describe_plan(inner)),
    }
}

fn describe_select(s: &SelectStmt) -> String {
    let mut lines: Vec<String> = Vec::new();
    match &s.from {
        None => lines.push("Const".into()),
        Some(from) => lines.extend(describe_from(from, 0)),
    }
    if s.r#where.is_some() {
        lines.push("  Filter (WHERE)".into());
    }
    if !s.group_by.is_empty() || items_have_aggregate(s) {
        if !s.group_by.is_empty() {
            lines.push(format!("  GroupBy ({} keys)", s.group_by.len()));
        } else {
            lines.push("  Aggregate (single group)".into());
        }
    }
    if s.having.is_some() {
        lines.push("  Filter (HAVING)".into());
    }
    if !s.order_by.is_empty() {
        lines.push(format!("  Sort ({} keys)", s.order_by.len()));
    }
    if s.distinct {
        lines.push("  Distinct".into());
    }
    if s.limit.is_some() || s.offset.is_some() {
        lines.push("  Limit / Offset".into());
    }
    lines.push(format!("  Project ({} items)", s.items.len()));
    for u in &s.unions {
        let kind = if u.all { "UNION ALL" } else { "UNION" };
        lines.push(format!("{kind}:"));
        for sub in describe_select(&u.query).lines() {
            lines.push(format!("  {sub}"));
        }
    }
    lines.join("\n")
}

fn describe_from(from: &FromClause, indent: usize) -> Vec<String> {
    let pad = "  ".repeat(indent);
    match from {
        FromClause::Table { name, alias } => {
            let alias = alias.as_deref().map(|a| format!(" AS {a}")).unwrap_or_default();
            vec![format!("{pad}Scan `{name}`{alias}")]
        }
        FromClause::Join { left, kind, right, .. } => {
            let mut out = vec![format!("{pad}{} Join", join_label(*kind))];
            out.extend(describe_from(left, indent + 1));
            out.extend(describe_from(right, indent + 1));
            out
        }
    }
}

fn join_label(k: JoinKind) -> &'static str {
    match k {
        JoinKind::Inner => "Inner",
        JoinKind::Left => "Left",
        JoinKind::Right => "Right",
    }
}

fn items_have_aggregate(s: &SelectStmt) -> bool {
    let mut tmp = Vec::new();
    for item in &s.items {
        if let SelectItem::Expr { expr, .. } = item
            && collect_aggregates(expr, &mut tmp).is_ok()
            && !tmp.is_empty()
        {
            return true;
        }
    }
    false
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

    // ----- DISTINCT ---------------------------------------------------

    // ----- NULLS FIRST/LAST -------------------------------------------

    // ----- UNION / UNION ALL -----------------------------------------

    #[test]
    fn union_all_keeps_duplicates() {
        let mut e = MemoryEngine::new();
        run_all(&mut e, "
            CREATE TABLE a (x INT PRIMARY KEY);
            CREATE TABLE b (y INT PRIMARY KEY);
            INSERT INTO a VALUES (1),(2),(3);
            INSERT INTO b VALUES (3),(4),(5);
        ");
        let r = run(&mut e, "SELECT x FROM a UNION ALL SELECT y FROM b").unwrap();
        let rows = assert_select(&r);
        assert_eq!(rows.len(), 6);
    }

    #[test]
    fn union_dedupes() {
        let mut e = MemoryEngine::new();
        run_all(&mut e, "
            CREATE TABLE a (x INT PRIMARY KEY);
            CREATE TABLE b (y INT PRIMARY KEY);
            INSERT INTO a VALUES (1),(2),(3);
            INSERT INTO b VALUES (3),(4),(5);
        ");
        let r = run(&mut e, "SELECT x FROM a UNION SELECT y FROM b").unwrap();
        let rows = assert_select(&r);
        assert_eq!(rows.len(), 5);
    }

    #[test]
    fn union_column_count_mismatch_errors() {
        let mut e = MemoryEngine::new();
        run_all(&mut e, "
            CREATE TABLE a (x INT PRIMARY KEY, y INT);
            CREATE TABLE b (z INT PRIMARY KEY);
            INSERT INTO a VALUES (1, 2);
            INSERT INTO b VALUES (3);
        ");
        let r = try_run(&mut e, "SELECT x, y FROM a UNION SELECT z FROM b");
        assert!(r.is_err());
    }

    // ----- Scalar subqueries -----------------------------------------

    #[test]
    fn scalar_subquery_in_where() {
        let mut e = MemoryEngine::new();
        run_all(&mut e, "
            CREATE TABLE t (id INT PRIMARY KEY, n INT);
            INSERT INTO t VALUES (1,10),(2,30),(3,20);
        ");
        let r = run(&mut e, "SELECT id FROM t WHERE n = (SELECT MAX(n) FROM t)").unwrap();
        let rows = assert_select(&r);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], Value::Integer(2));
    }

    #[test]
    fn scalar_subquery_in_select() {
        let mut e = MemoryEngine::new();
        run_all(&mut e, "
            CREATE TABLE t (id INT PRIMARY KEY, n INT);
            INSERT INTO t VALUES (1,10),(2,20),(3,30);
        ");
        let r = run(&mut e, "SELECT id, n - (SELECT AVG(n) FROM t) AS diff FROM t ORDER BY id").unwrap();
        let rows = assert_select(&r);
        // avg = 20 → diffs are -10, 0, 10 (as floats)
        assert_eq!(rows[0][1], Value::Float(-10.0));
        assert_eq!(rows[1][1], Value::Float(0.0));
        assert_eq!(rows[2][1], Value::Float(10.0));
    }

    #[test]
    fn scalar_subquery_zero_rows_yields_null() {
        let mut e = MemoryEngine::new();
        run_all(&mut e, "
            CREATE TABLE t (id INT PRIMARY KEY, n INT);
            INSERT INTO t VALUES (1,10);
            CREATE TABLE empty (id INT PRIMARY KEY);
        ");
        let r = run(&mut e, "SELECT (SELECT id FROM empty) AS v").unwrap();
        let rows = assert_select(&r);
        assert_eq!(rows[0][0], Value::Null);
    }

    #[test]
    fn scalar_subquery_multiple_rows_errors() {
        let mut e = MemoryEngine::new();
        run_all(&mut e, "
            CREATE TABLE t (id INT PRIMARY KEY);
            INSERT INTO t VALUES (1),(2),(3);
        ");
        let r = try_run(&mut e, "SELECT (SELECT id FROM t) AS v");
        assert!(r.is_err());
    }

    // ----- ALTER TABLE ADD COLUMN -------------------------------------

    #[test]
    fn alter_table_add_column_with_default() {
        let mut e = MemoryEngine::new();
        run_all(&mut e, "
            CREATE TABLE t (id INT PRIMARY KEY, name TEXT);
            INSERT INTO t VALUES (1, 'a'), (2, 'b');
            ALTER TABLE t ADD COLUMN age INT DEFAULT 99;
        ");
        let r = run(&mut e, "SELECT id, name, age FROM t ORDER BY id").unwrap();
        let rows = assert_select(&r);
        assert_eq!(rows[0][2], Value::Integer(99));
        assert_eq!(rows[1][2], Value::Integer(99));
    }

    #[test]
    fn alter_table_add_column_nullable_no_default() {
        let mut e = MemoryEngine::new();
        run_all(&mut e, "
            CREATE TABLE t (id INT PRIMARY KEY);
            INSERT INTO t VALUES (1);
            ALTER TABLE t ADD email TEXT;
        ");
        let r = run(&mut e, "SELECT id, email FROM t").unwrap();
        let rows = assert_select(&r);
        assert_eq!(rows[0][1], Value::Null);
    }

    #[test]
    fn alter_table_add_not_null_without_default_rejected() {
        let mut e = MemoryEngine::new();
        run_all(&mut e, "
            CREATE TABLE t (id INT PRIMARY KEY);
            INSERT INTO t VALUES (1);
        ");
        let r = try_run(&mut e, "ALTER TABLE t ADD COLUMN x INT NOT NULL");
        assert!(r.is_err(), "expected error: {r:?}");
    }

    #[test]
    fn alter_table_duplicate_column_rejected() {
        let mut e = MemoryEngine::new();
        run_all(&mut e, "CREATE TABLE t (id INT PRIMARY KEY, name TEXT)");
        let r = try_run(&mut e, "ALTER TABLE t ADD COLUMN name TEXT");
        assert!(r.is_err());
    }

    fn try_run(e: &mut MemoryEngine, sql: &str) -> Result<ResultSet> {
        let stmt = Parser::parse_one(sql)?;
        Executor::new(e).execute(&stmt)
    }

    #[test]
    fn order_by_default_null_placement() {
        let mut e = MemoryEngine::new();
        run_all(&mut e, "
            CREATE TABLE t (id INT PRIMARY KEY, n INT);
            INSERT INTO t VALUES (1,1),(2,NULL),(3,3),(4,NULL);
        ");
        // ASC default: NULLS LAST
        let r = run(&mut e, "SELECT id FROM t ORDER BY n ASC").unwrap();
        let rows = assert_select(&r);
        // First two rows should be the non-null ones (id 1, 3) in ascending order.
        assert_eq!(rows[0][0], Value::Integer(1));
        assert_eq!(rows[1][0], Value::Integer(3));
        // Last two are the NULL rows.
        // DESC default: NULLS FIRST
        let r = run(&mut e, "SELECT id FROM t ORDER BY n DESC").unwrap();
        let rows = assert_select(&r);
        // First two rows should be the NULL ones.
        assert!(matches!(rows[0][0], Value::Integer(2 | 4)));
        assert!(matches!(rows[1][0], Value::Integer(2 | 4)));
    }

    #[test]
    fn order_by_explicit_nulls_first() {
        let mut e = MemoryEngine::new();
        run_all(&mut e, "
            CREATE TABLE t (id INT PRIMARY KEY, n INT);
            INSERT INTO t VALUES (1,1),(2,NULL),(3,3);
        ");
        let r = run(&mut e, "SELECT id FROM t ORDER BY n ASC NULLS FIRST").unwrap();
        let rows = assert_select(&r);
        assert_eq!(rows[0][0], Value::Integer(2)); // NULL row first
        assert_eq!(rows[1][0], Value::Integer(1));
        assert_eq!(rows[2][0], Value::Integer(3));
    }

    #[test]
    fn select_distinct_dedupes() {
        let mut e = MemoryEngine::new();
        run_all(&mut e, "
            CREATE TABLE t (id INT PRIMARY KEY, c TEXT);
            INSERT INTO t VALUES (1,'a'),(2,'b'),(3,'a'),(4,'b'),(5,'c');
        ");
        let r = run(&mut e, "SELECT DISTINCT c FROM t ORDER BY c").unwrap();
        let rows = assert_select(&r);
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn select_distinct_with_limit() {
        let mut e = MemoryEngine::new();
        run_all(&mut e, "
            CREATE TABLE t (id INT PRIMARY KEY, c TEXT);
            INSERT INTO t VALUES (1,'a'),(2,'b'),(3,'a'),(4,'c');
        ");
        let r = run(&mut e, "SELECT DISTINCT c FROM t ORDER BY c LIMIT 2").unwrap();
        let rows = assert_select(&r);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn count_distinct_dedupes_values() {
        let mut e = MemoryEngine::new();
        run_all(&mut e, "
            CREATE TABLE t (id INT PRIMARY KEY, c TEXT);
            INSERT INTO t VALUES (1,'a'),(2,'b'),(3,'a'),(4,'a'),(5,'c'),(6,NULL);
        ");
        let r = run(&mut e, "SELECT COUNT(*), COUNT(c), COUNT(DISTINCT c) FROM t").unwrap();
        let rows = assert_select(&r);
        assert_eq!(rows[0][0], Value::Integer(6)); // all rows
        assert_eq!(rows[0][1], Value::Integer(5)); // non-null
        assert_eq!(rows[0][2], Value::Integer(3)); // a, b, c
    }

    #[test]
    fn sum_distinct_dedupes() {
        let mut e = MemoryEngine::new();
        run_all(&mut e, "
            CREATE TABLE t (id INT PRIMARY KEY, n INT);
            INSERT INTO t VALUES (1,5),(2,5),(3,7),(4,5);
        ");
        let r = run(&mut e, "SELECT SUM(n), SUM(DISTINCT n) FROM t").unwrap();
        let rows = assert_select(&r);
        assert_eq!(rows[0][0], Value::Integer(22)); // 5+5+7+5
        assert_eq!(rows[0][1], Value::Integer(12)); // 5+7
    }

    #[test]
    fn select_distinct_with_aggregate() {
        let mut e = MemoryEngine::new();
        run_all(&mut e, "
            CREATE TABLE t (id INT PRIMARY KEY, city TEXT, n INT);
            INSERT INTO t VALUES (1,'x',1),(2,'x',2),(3,'y',3);
        ");
        // After GROUP BY each (city) yields one row anyway. DISTINCT is
        // a no-op but should still parse and execute.
        let r = run(&mut e, "SELECT DISTINCT city FROM t GROUP BY city ORDER BY city").unwrap();
        let rows = assert_select(&r);
        assert_eq!(rows.len(), 2);
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
