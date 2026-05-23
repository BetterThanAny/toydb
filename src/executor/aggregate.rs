//! Aggregate functions and grouped projection.
//!
//! `find_aggregates` walks an expression tree and returns one entry per
//! aggregate call (COUNT, SUM, AVG, MIN, MAX). Each gets a stable
//! [`AggKey`] derived from `(name, args)` so the same aggregate
//! reused in SELECT and HAVING shares one accumulator.
//!
//! [`Accumulator`] holds the running state for one group + one aggregate.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::error::{Error, Result};
use crate::executor::expr::{Resolver, eval_with};
use crate::sql::ast::{Expression, Literal};
use crate::types::value::Value;

/// A canonical key for an aggregate expression. We use the original
/// `Expression` directly — it implements `PartialEq`, and structural
/// equality is exactly what we want (`SUM(price) == SUM(price)`).
#[derive(Debug, Clone, PartialEq)]
pub struct AggKey(pub Expression);

impl AggKey {
    pub fn matches(&self, expr: &Expression) -> bool {
        &self.0 == expr
    }
}

/// All five SQL aggregate kinds plus a source argument expression.
#[derive(Debug, Clone)]
pub struct AggSpec {
    pub kind: AggKind,
    /// `COUNT(*)` carries `Expression::Wildcard` so we know to count
    /// every input row (NULL or not).
    pub arg: Expression,
    /// `true` for `COUNT(DISTINCT col)` etc. — the accumulator only
    /// records each *unique* value once.
    pub distinct: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggKind {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl AggKind {
    pub fn name(self) -> &'static str {
        match self {
            AggKind::Count => "COUNT",
            AggKind::Sum => "SUM",
            AggKind::Avg => "AVG",
            AggKind::Min => "MIN",
            AggKind::Max => "MAX",
        }
    }
}

/// Recognised aggregate function names → their kind.
pub fn aggregate_kind(name: &str) -> Option<AggKind> {
    Some(match name.to_ascii_uppercase().as_str() {
        "COUNT" => AggKind::Count,
        "SUM" => AggKind::Sum,
        "AVG" => AggKind::Avg,
        "MIN" => AggKind::Min,
        "MAX" => AggKind::Max,
        _ => return None,
    })
}

/// Walk `expr` and collect every aggregate call. Duplicates are
/// dropped (we keep the first occurrence keyed by the AST). Aggregates
/// nested inside aggregates are rejected.
pub fn collect_aggregates(expr: &Expression, out: &mut Vec<(AggKey, AggSpec)>) -> Result<()> {
    collect_inner(expr, out, false)
}

fn collect_inner(
    expr: &Expression,
    out: &mut Vec<(AggKey, AggSpec)>,
    inside_agg: bool,
) -> Result<()> {
    match expr {
        Expression::Function {
            name,
            args,
            distinct,
        } if aggregate_kind(name).is_some() => {
            if inside_agg {
                return Err(Error::other("aggregates cannot be nested"));
            }
            let kind = aggregate_kind(name).expect("checked above");
            match (kind, args.len()) {
                (AggKind::Count, 1) | (_, 1) => {}
                (k, n) => {
                    return Err(Error::ty(format!("{} takes 1 argument, got {n}", k.name())));
                }
            }
            for a in args {
                collect_inner(a, out, true)?;
            }
            let arg = args[0].clone();
            let key = AggKey(expr.clone());
            if !out.iter().any(|(k, _)| k.matches(expr)) {
                out.push((
                    key,
                    AggSpec {
                        kind,
                        arg,
                        distinct: *distinct,
                    },
                ));
            }
        }
        Expression::Function { args, distinct, .. } => {
            if *distinct {
                return Err(Error::ty(
                    "DISTINCT is only valid inside an aggregate function".to_string(),
                ));
            }
            for a in args {
                collect_inner(a, out, inside_agg)?;
            }
        }
        Expression::Unary(_, inner) => collect_inner(inner, out, inside_agg)?,
        Expression::Binary(l, _, r) => {
            collect_inner(l, out, inside_agg)?;
            collect_inner(r, out, inside_agg)?;
        }
        Expression::IsNull { expr, .. } => collect_inner(expr, out, inside_agg)?,
        Expression::InList { expr, list, .. } => {
            collect_inner(expr, out, inside_agg)?;
            for item in list {
                collect_inner(item, out, inside_agg)?;
            }
        }
        Expression::Between {
            expr, low, high, ..
        } => {
            collect_inner(expr, out, inside_agg)?;
            collect_inner(low, out, inside_agg)?;
            collect_inner(high, out, inside_agg)?;
        }
        Expression::Like { expr, pattern, .. } => {
            collect_inner(expr, out, inside_agg)?;
            collect_inner(pattern, out, inside_agg)?;
        }
        Expression::Case {
            operand,
            branches,
            otherwise,
        } => {
            if let Some(op) = operand {
                collect_inner(op, out, inside_agg)?;
            }
            for (w, t) in branches {
                collect_inner(w, out, inside_agg)?;
                collect_inner(t, out, inside_agg)?;
            }
            if let Some(e) = otherwise {
                collect_inner(e, out, inside_agg)?;
            }
        }
        Expression::Scalar(_) => {
            // Scalar subqueries are resolved before aggregation; nothing
            // to recurse into here.
        }
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Accumulators
// ---------------------------------------------------------------------

/// Mutable folding state for one aggregate over one group.
#[derive(Debug, Clone)]
pub struct Accumulator {
    kind: AggKind,
    distinct: bool,
    seen_values: std::collections::BTreeSet<GroupKey>,
    count: i64,
    sum_int: Option<i64>,
    sum_float: Option<f64>,
    extremum: Option<Value>,
    /// `true` when at least one input row produced a usable value.
    seen: bool,
}

impl Accumulator {
    pub fn new(kind: AggKind) -> Self {
        Self::new_with_distinct(kind, false)
    }

    pub fn new_with_distinct(kind: AggKind, distinct: bool) -> Self {
        Self {
            kind,
            distinct,
            seen_values: std::collections::BTreeSet::new(),
            count: 0,
            sum_int: Some(0),
            sum_float: None,
            extremum: None,
            seen: false,
        }
    }

    pub fn update(&mut self, value: Value) -> Result<()> {
        // For DISTINCT aggregates, drop duplicates (and NULL is implicitly
        // excluded by the same path as non-distinct aggregates).
        if self.distinct {
            if value.is_null() {
                return Ok(());
            }
            let key = GroupKey(vec![value.clone()]);
            if !self.seen_values.insert(key) {
                return Ok(());
            }
        }
        match self.kind {
            AggKind::Count => {
                if !value.is_null() {
                    self.count += 1;
                    self.seen = true;
                }
            }
            AggKind::Sum | AggKind::Avg => {
                if value.is_null() {
                    return Ok(());
                }
                self.seen = true;
                self.count += 1;
                match value {
                    Value::Integer(n) => match (self.sum_int, self.sum_float) {
                        (Some(acc), None) => self.sum_int = Some(acc.wrapping_add(n)),
                        (_, Some(acc)) => self.sum_float = Some(acc + n as f64),
                        _ => unreachable!(),
                    },
                    Value::Float(f) => {
                        let prior = self.sum_float.unwrap_or_else(|| {
                            self.sum_int.take().map(|n| n as f64).unwrap_or(0.0)
                        });
                        self.sum_float = Some(prior + f);
                    }
                    other => {
                        return Err(Error::ty(format!(
                            "{} expects numeric, got {}",
                            self.kind.name(),
                            other.type_name()
                        )));
                    }
                }
            }
            AggKind::Min | AggKind::Max => {
                if value.is_null() {
                    return Ok(());
                }
                self.seen = true;
                self.extremum = Some(match self.extremum.take() {
                    None => value,
                    Some(prev) => {
                        let cmp = prev.partial_cmp_sql(&value)?.unwrap_or(Ordering::Equal);
                        let pick_new = match self.kind {
                            AggKind::Min => cmp == Ordering::Greater,
                            AggKind::Max => cmp == Ordering::Less,
                            _ => false,
                        };
                        if pick_new { value } else { prev }
                    }
                });
            }
        }
        Ok(())
    }

    pub fn finalize(&self) -> Value {
        match self.kind {
            AggKind::Count => Value::Integer(self.count),
            AggKind::Sum => {
                if !self.seen {
                    return Value::Null;
                }
                if let Some(f) = self.sum_float {
                    Value::Float(f)
                } else {
                    Value::Integer(self.sum_int.unwrap_or(0))
                }
            }
            AggKind::Avg => {
                if !self.seen || self.count == 0 {
                    return Value::Null;
                }
                let total = match self.sum_float {
                    Some(f) => f,
                    None => self.sum_int.unwrap_or(0) as f64,
                };
                Value::Float(total / self.count as f64)
            }
            AggKind::Min | AggKind::Max => self.extremum.clone().unwrap_or(Value::Null),
        }
    }
}

// ---------------------------------------------------------------------
// Group key
// ---------------------------------------------------------------------

/// A `Vec<Value>` wrapped so we can use it as a `BTreeMap` key without
/// requiring `Value: Ord`. The internal ordering uses `Value::total_cmp`,
/// which gives a total order even though `Value` only implements
/// `PartialEq` (because of `f64`).
#[derive(Debug, Clone)]
pub struct GroupKey(pub Vec<Value>);

impl PartialEq for GroupKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for GroupKey {}

impl Ord for GroupKey {
    fn cmp(&self, other: &Self) -> Ordering {
        for (a, b) in self.0.iter().zip(other.0.iter()) {
            let c = a.total_cmp(b);
            if c != Ordering::Equal {
                return c;
            }
        }
        self.0.len().cmp(&other.0.len())
    }
}

impl PartialOrd for GroupKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Map of group key → per-aggregate accumulators.
pub type Groups = BTreeMap<GroupKey, Vec<Accumulator>>;

/// Build a starting accumulator vector matching `aggs`.
pub fn fresh_accumulators(aggs: &[AggSpec]) -> Vec<Accumulator> {
    aggs.iter()
        .map(|a| Accumulator::new_with_distinct(a.kind, a.distinct))
        .collect()
}

/// Update one group's accumulators with one input row.
pub fn fold_row<R: Resolver + ?Sized>(
    accs: &mut [Accumulator],
    aggs: &[AggSpec],
    resolver: &R,
) -> Result<()> {
    for (acc, spec) in accs.iter_mut().zip(aggs.iter()) {
        let v = match (&spec.arg, spec.kind) {
            (Expression::Wildcard, AggKind::Count) => Value::Integer(1),
            (Expression::Wildcard, _) => {
                return Err(Error::ty(format!("{} cannot take *", spec.kind.name())));
            }
            (e, _) => eval_with(e, resolver)?,
        };
        acc.update(v)?;
    }
    Ok(())
}

/// Evaluate `expr` in a context where aggregate calls resolve to their
/// finalised slot values. Non-aggregate sub-expressions evaluate via
/// `outer` (which reads group keys / literals).
pub fn eval_in_group<R: Resolver + ?Sized>(
    expr: &Expression,
    aggs: &[(AggKey, AggSpec)],
    finals: &[Value],
    outer: &R,
) -> Result<Value> {
    if let Some(idx) = aggs.iter().position(|(k, _)| k.matches(expr)) {
        return Ok(finals[idx].clone());
    }
    match expr {
        Expression::Literal(l) => Ok(literal_value(l)),
        Expression::Column(_) | Expression::Qualified(_, _) => eval_with(expr, outer),
        Expression::Wildcard => Err(Error::internal("`*` cannot appear here")),
        Expression::Unary(op, inner) => {
            let v = eval_in_group(inner, aggs, finals, outer)?;
            // delegate to scalar evaluator's apply_unary by re-using eval_with
            // — easiest: build a literal wrapper and call eval_with.
            let lit = literal_to_expr(v);
            eval_with(&Expression::Unary(*op, Box::new(lit)), outer)
        }
        Expression::Binary(l, op, r) => {
            let lv = eval_in_group(l, aggs, finals, outer)?;
            let rv = eval_in_group(r, aggs, finals, outer)?;
            eval_with(
                &Expression::Binary(
                    Box::new(literal_to_expr(lv)),
                    *op,
                    Box::new(literal_to_expr(rv)),
                ),
                outer,
            )
        }
        Expression::IsNull { expr, negated } => {
            let v = eval_in_group(expr, aggs, finals, outer)?;
            Ok(Value::Boolean(if *negated {
                !v.is_null()
            } else {
                v.is_null()
            }))
        }
        Expression::InList {
            expr,
            list,
            negated,
        } => {
            let needle = eval_in_group(expr, aggs, finals, outer)?;
            let mut found = false;
            let mut saw_null = false;
            for item in list {
                let v = eval_in_group(item, aggs, finals, outer)?;
                if v.is_null() {
                    saw_null = true;
                    continue;
                }
                if let Some(true) = needle.equal_sql(&v)? {
                    found = true;
                    break;
                }
            }
            if needle.is_null() {
                return Ok(Value::Null);
            }
            let result = if found {
                Some(true)
            } else if saw_null {
                None
            } else {
                Some(false)
            };
            Ok(match result {
                None => Value::Null,
                Some(b) => Value::Boolean(if *negated { !b } else { b }),
            })
        }
        Expression::Between {
            expr,
            low,
            high,
            negated,
        } => {
            let v = eval_in_group(expr, aggs, finals, outer)?;
            let lo = eval_in_group(low, aggs, finals, outer)?;
            let hi = eval_in_group(high, aggs, finals, outer)?;
            if v.is_null() || lo.is_null() || hi.is_null() {
                return Ok(Value::Null);
            }
            let cmp_lo = v.partial_cmp_sql(&lo)?;
            let cmp_hi = v.partial_cmp_sql(&hi)?;
            let in_range =
                matches!(cmp_lo, Some(o) if o.is_ge()) && matches!(cmp_hi, Some(o) if o.is_le());
            Ok(Value::Boolean(if *negated { !in_range } else { in_range }))
        }
        Expression::Like {
            expr,
            pattern,
            negated,
        } => {
            let v = eval_in_group(expr, aggs, finals, outer)?;
            let p = eval_in_group(pattern, aggs, finals, outer)?;
            if v.is_null() || p.is_null() {
                return Ok(Value::Null);
            }
            let s = match v {
                Value::String(s) => s,
                other => {
                    return Err(Error::ty(format!(
                        "LIKE expects string, got {}",
                        other.type_name()
                    )));
                }
            };
            let pat = match p {
                Value::String(s) => s,
                other => {
                    return Err(Error::ty(format!(
                        "LIKE pattern must be string, got {}",
                        other.type_name()
                    )));
                }
            };
            let m = crate::executor::expr::like_match_for_test(&s, &pat);
            Ok(Value::Boolean(if *negated { !m } else { m }))
        }
        Expression::Function {
            name,
            args,
            distinct,
        } => {
            let new_args: Vec<Expression> = args
                .iter()
                .map(|a| eval_in_group(a, aggs, finals, outer).map(literal_to_expr))
                .collect::<Result<_>>()?;
            eval_with(
                &Expression::Function {
                    name: name.clone(),
                    args: new_args,
                    distinct: *distinct,
                },
                outer,
            )
        }
        Expression::Scalar(_) => Err(Error::internal(
            "scalar subqueries must be resolved before eval_in_group (executor bug)",
        )),
        Expression::Case {
            operand,
            branches,
            otherwise,
        } => {
            match operand {
                Some(op) => {
                    let target = eval_in_group(op, aggs, finals, outer)?;
                    for (when, then) in branches {
                        let candidate = eval_in_group(when, aggs, finals, outer)?;
                        if let Some(true) = target.equal_sql(&candidate)? {
                            return eval_in_group(then, aggs, finals, outer);
                        }
                    }
                }
                None => {
                    for (when, then) in branches {
                        match eval_in_group(when, aggs, finals, outer)? {
                            Value::Boolean(true) => {
                                return eval_in_group(then, aggs, finals, outer);
                            }
                            Value::Boolean(false) | Value::Null => continue,
                            other => {
                                return Err(Error::ty(format!(
                                    "CASE WHEN expects boolean, got {}",
                                    other.type_name()
                                )));
                            }
                        }
                    }
                }
            }
            match otherwise {
                Some(e) => eval_in_group(e, aggs, finals, outer),
                None => Ok(Value::Null),
            }
        }
    }
}

fn literal_value(lit: &Literal) -> Value {
    match lit {
        Literal::Null => Value::Null,
        Literal::Boolean(b) => Value::Boolean(*b),
        Literal::Integer(n) => Value::Integer(*n),
        Literal::Float(f) => Value::Float(*f),
        Literal::String(s) => Value::String(s.clone()),
    }
}

fn literal_to_expr(v: Value) -> Expression {
    match v {
        Value::Null => Expression::Literal(Literal::Null),
        Value::Boolean(b) => Expression::Literal(Literal::Boolean(b)),
        Value::Integer(n) => Expression::Literal(Literal::Integer(n)),
        Value::Float(f) => Expression::Literal(Literal::Float(f)),
        Value::String(s) => Expression::Literal(Literal::String(s)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::Parser;

    fn expr(s: &str) -> Expression {
        let mut p = Parser::new(s).unwrap();
        p.parse_expression().unwrap()
    }

    #[test]
    fn extract_simple() {
        let e = expr("COUNT(*)");
        let mut v = Vec::new();
        collect_aggregates(&e, &mut v).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].1.kind, AggKind::Count);
    }

    #[test]
    fn extract_dedupes() {
        let e = expr("SUM(price) + SUM(price) * 2");
        let mut v = Vec::new();
        collect_aggregates(&e, &mut v).unwrap();
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn extract_distinct_aggs() {
        let e = expr("AVG(x) + MIN(x) + MAX(x)");
        let mut v = Vec::new();
        collect_aggregates(&e, &mut v).unwrap();
        assert_eq!(v.len(), 3);
    }

    #[test]
    fn nested_aggs_rejected() {
        let e = expr("SUM(COUNT(x))");
        let mut v = Vec::new();
        assert!(collect_aggregates(&e, &mut v).is_err());
    }

    #[test]
    fn count_skips_null() {
        let mut acc = Accumulator::new(AggKind::Count);
        acc.update(Value::Null).unwrap();
        acc.update(Value::Integer(1)).unwrap();
        acc.update(Value::Null).unwrap();
        acc.update(Value::Integer(2)).unwrap();
        assert_eq!(acc.finalize(), Value::Integer(2));
    }

    #[test]
    fn sum_int_then_float_promotes() {
        let mut acc = Accumulator::new(AggKind::Sum);
        acc.update(Value::Integer(1)).unwrap();
        acc.update(Value::Integer(2)).unwrap();
        acc.update(Value::Float(0.5)).unwrap();
        assert_eq!(acc.finalize(), Value::Float(3.5));
    }

    #[test]
    fn avg_with_null_uses_count_excluding_null() {
        let mut acc = Accumulator::new(AggKind::Avg);
        acc.update(Value::Integer(2)).unwrap();
        acc.update(Value::Null).unwrap();
        acc.update(Value::Integer(4)).unwrap();
        // (2+4)/2 = 3.0
        assert_eq!(acc.finalize(), Value::Float(3.0));
    }

    #[test]
    fn min_max_handles_null() {
        let mut acc = Accumulator::new(AggKind::Min);
        acc.update(Value::Integer(3)).unwrap();
        acc.update(Value::Integer(1)).unwrap();
        acc.update(Value::Null).unwrap();
        assert_eq!(acc.finalize(), Value::Integer(1));

        let mut acc = Accumulator::new(AggKind::Max);
        acc.update(Value::String("b".into())).unwrap();
        acc.update(Value::String("z".into())).unwrap();
        acc.update(Value::String("a".into())).unwrap();
        assert_eq!(acc.finalize(), Value::String("z".into()));
    }

    #[test]
    fn empty_aggregates_emit_null_or_zero() {
        let acc = Accumulator::new(AggKind::Count);
        assert_eq!(acc.finalize(), Value::Integer(0));
        let acc = Accumulator::new(AggKind::Sum);
        assert_eq!(acc.finalize(), Value::Null);
        let acc = Accumulator::new(AggKind::Avg);
        assert_eq!(acc.finalize(), Value::Null);
        let acc = Accumulator::new(AggKind::Min);
        assert_eq!(acc.finalize(), Value::Null);
    }

    #[test]
    fn group_key_total_cmp() {
        let a = GroupKey(vec![Value::Integer(1), Value::String("a".into())]);
        let b = GroupKey(vec![Value::Integer(1), Value::String("b".into())]);
        assert!(a < b);
    }
}
