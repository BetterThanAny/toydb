//! Scalar expression evaluator.
//!
//! Walks an [`Expression`] node and produces a [`Value`] using a row +
//! resolver protocol. The resolver decides how column references are
//! resolved (single-table executor uses positional lookup; join
//! executor merges multiple rows under aliased prefixes).
//!
//! Aggregate functions (`COUNT`, `SUM`, `AVG`, `MIN`, `MAX`) are not
//! handled here — they are recognised by the planner and rewritten into
//! aggregation operators that feed the evaluator literal values.

use crate::error::{Error, Result};
use crate::sql::ast::{BinaryOp, Expression, Literal, UnaryOp};
use crate::types::value::Value;

// ---------------------------------------------------------------------
// Resolver
// ---------------------------------------------------------------------

/// Strategy for turning column references into values. Trait so the
/// caller can pick row-on-the-fly resolution (`HashMap`-based for joins)
/// vs. positional resolution (single-table scan).
pub trait Resolver {
    /// Resolve a bare column name (`SELECT name`).
    fn column(&self, name: &str) -> Result<Value>;
    /// Resolve `table.col`.
    fn qualified(&self, table: &str, name: &str) -> Result<Value>;
}

/// Trivial resolver that returns an error for any column reference —
/// useful when evaluating literal-only expressions (e.g. constant
/// folding in `INSERT ... VALUES`).
pub struct EmptyResolver;
impl Resolver for EmptyResolver {
    fn column(&self, name: &str) -> Result<Value> {
        Err(Error::ty(format!(
            "column `{name}` referenced outside any row"
        )))
    }
    fn qualified(&self, t: &str, c: &str) -> Result<Value> {
        Err(Error::ty(format!(
            "column `{t}.{c}` referenced outside any row"
        )))
    }
}

// ---------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------

/// Evaluate an expression that contains no column references.
pub fn eval(expr: &Expression) -> Result<Value> {
    eval_with(expr, &EmptyResolver)
}

/// Evaluate an expression in the context of a [`Resolver`].
pub fn eval_with<R: Resolver + ?Sized>(expr: &Expression, r: &R) -> Result<Value> {
    match expr {
        Expression::Literal(lit) => Ok(literal_value(lit)),
        Expression::Column(name) => r.column(name),
        Expression::Qualified(t, c) => r.qualified(t, c),
        Expression::Wildcard => Err(Error::internal("`*` cannot appear here")),
        Expression::Unary(op, inner) => {
            let v = eval_with(inner, r)?;
            apply_unary(*op, v)
        }
        Expression::Binary(l, op, rhs) => {
            // Short-circuit AND/OR for both performance and SQL semantics.
            match op {
                BinaryOp::And => {
                    let lv = eval_with(l, r)?;
                    if matches!(lv, Value::Boolean(false)) {
                        validate_short_circuit_boolean_operand(rhs, r, "AND")?;
                        return Ok(Value::Boolean(false));
                    }
                    let rv = eval_with(rhs, r)?;
                    return apply_and(lv, rv);
                }
                BinaryOp::Or => {
                    let lv = eval_with(l, r)?;
                    if matches!(lv, Value::Boolean(true)) {
                        validate_short_circuit_boolean_operand(rhs, r, "OR")?;
                        return Ok(Value::Boolean(true));
                    }
                    let rv = eval_with(rhs, r)?;
                    return apply_or(lv, rv);
                }
                _ => {}
            }
            let lv = eval_with(l, r)?;
            let rv = eval_with(rhs, r)?;
            apply_binary(*op, lv, rv)
        }
        Expression::IsNull { expr, negated } => {
            let v = eval_with(expr, r)?;
            let is_null = v.is_null();
            Ok(Value::Boolean(if *negated { !is_null } else { is_null }))
        }
        Expression::InList {
            expr,
            list,
            negated,
        } => {
            let needle = eval_with(expr, r)?;
            if needle.is_null() {
                return Ok(Value::Null);
            }
            let mut found = false;
            let mut saw_null = false;
            for item in list {
                let v = eval_with(item, r)?;
                if v.is_null() {
                    saw_null = true;
                    continue;
                }
                if let Some(true) = needle.equal_sql(&v)? {
                    found = true;
                    break;
                }
            }
            // SQL semantics:
            //   x IN (NULL)   -> NULL if x not present
            //   x IN (a)  matches -> TRUE; doesn't match -> FALSE
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
            let v = eval_with(expr, r)?;
            let lo = eval_with(low, r)?;
            let hi = eval_with(high, r)?;
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
            let v = eval_with(expr, r)?;
            let p = eval_with(pattern, r)?;
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
            let m = like_match(&s, &pat);
            Ok(Value::Boolean(if *negated { !m } else { m }))
        }
        Expression::Function {
            name,
            args,
            distinct,
        } => {
            if *distinct {
                return Err(Error::ty(format!(
                    "DISTINCT is only supported inside aggregate calls, not `{name}`"
                )));
            }
            apply_function(name, args, r)
        }
        Expression::Scalar(_) => Err(Error::internal(
            "scalar subqueries must be resolved before eval (executor bug)",
        )),
        Expression::Case {
            operand,
            branches,
            otherwise,
        } => {
            // Switch form evaluates `operand` once and compares it against
            // each WHEN expression with SQL equality (NULL never matches).
            // Boolean form treats each WHEN as a predicate.
            match operand {
                Some(op) => {
                    let target = eval_with(op, r)?;
                    for (when, then) in branches {
                        let candidate = eval_with(when, r)?;
                        if let Some(true) = target.equal_sql(&candidate)? {
                            return eval_with(then, r);
                        }
                    }
                }
                None => {
                    for (when, then) in branches {
                        match eval_with(when, r)? {
                            Value::Boolean(true) => return eval_with(then, r),
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
                Some(e) => eval_with(e, r),
                None => Ok(Value::Null),
            }
        }
    }
}

// ---------------------------------------------------------------------
// Operators
// ---------------------------------------------------------------------

fn literal_value(lit: &Literal) -> Value {
    match lit {
        Literal::Null => Value::Null,
        Literal::Boolean(b) => Value::Boolean(*b),
        Literal::Integer(n) => Value::Integer(*n),
        Literal::Float(f) => Value::Float(*f),
        Literal::String(s) => Value::String(s.clone()),
    }
}

fn apply_unary(op: UnaryOp, v: Value) -> Result<Value> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    Ok(match (op, v) {
        (UnaryOp::Plus, v @ (Value::Integer(_) | Value::Float(_))) => v,
        (UnaryOp::Minus, Value::Integer(n)) => Value::Integer(
            n.checked_neg()
                .ok_or_else(|| Error::value("integer negation overflow"))?,
        ),
        (UnaryOp::Minus, Value::Float(f)) => Value::Float(-f),
        (UnaryOp::Not, Value::Boolean(b)) => Value::Boolean(!b),
        (op, v) => {
            return Err(Error::ty(format!(
                "cannot apply `{op:?}` to {}",
                v.type_name()
            )));
        }
    })
}

fn apply_binary(op: BinaryOp, l: Value, r: Value) -> Result<Value> {
    if matches!(
        op,
        BinaryOp::Eq
            | BinaryOp::NotEq
            | BinaryOp::Lt
            | BinaryOp::LtEq
            | BinaryOp::Gt
            | BinaryOp::GtEq
    ) {
        return apply_comparison(op, l, r);
    }
    if l.is_null() || r.is_null() {
        return Ok(Value::Null);
    }
    Ok(match (op, l, r) {
        // Arithmetic
        (BinaryOp::Add, Value::Integer(a), Value::Integer(b)) => Value::Integer(
            a.checked_add(b)
                .ok_or_else(|| Error::value("integer addition overflow"))?,
        ),
        (BinaryOp::Sub, Value::Integer(a), Value::Integer(b)) => Value::Integer(
            a.checked_sub(b)
                .ok_or_else(|| Error::value("integer subtraction overflow"))?,
        ),
        (BinaryOp::Mul, Value::Integer(a), Value::Integer(b)) => Value::Integer(
            a.checked_mul(b)
                .ok_or_else(|| Error::value("integer multiplication overflow"))?,
        ),
        (BinaryOp::Div, Value::Integer(a), Value::Integer(b)) => {
            if b == 0 {
                return Err(Error::value("integer division by zero"));
            }
            Value::Integer(
                a.checked_div(b)
                    .ok_or_else(|| Error::value("integer division overflow"))?,
            )
        }
        (BinaryOp::Mod, Value::Integer(a), Value::Integer(b)) => {
            if b == 0 {
                return Err(Error::value("integer modulo by zero"));
            }
            Value::Integer(
                a.checked_rem(b)
                    .ok_or_else(|| Error::value("integer modulo overflow"))?,
            )
        }
        (BinaryOp::Pow, Value::Integer(a), Value::Integer(b)) => {
            if b < 0 {
                let exp =
                    i32::try_from(b).map_err(|_| Error::value("integer exponent out of range"))?;
                Value::Float((a as f64).powi(exp))
            } else {
                let exp =
                    u32::try_from(b).map_err(|_| Error::value("integer exponent out of range"))?;
                Value::Integer(
                    a.checked_pow(exp)
                        .ok_or_else(|| Error::value("integer exponent overflow"))?,
                )
            }
        }
        (BinaryOp::Add, l, r) if numeric(&l) && numeric(&r) => Value::Float(to_f64(l) + to_f64(r)),
        (BinaryOp::Sub, l, r) if numeric(&l) && numeric(&r) => Value::Float(to_f64(l) - to_f64(r)),
        (BinaryOp::Mul, l, r) if numeric(&l) && numeric(&r) => Value::Float(to_f64(l) * to_f64(r)),
        (BinaryOp::Div, l, r) if numeric(&l) && numeric(&r) => {
            let rhs = to_f64(r);
            if rhs == 0.0 {
                return Err(Error::value("division by zero"));
            }
            Value::Float(to_f64(l) / rhs)
        }
        (BinaryOp::Mod, l, r) if numeric(&l) && numeric(&r) => {
            let rhs = to_f64(r);
            if rhs == 0.0 {
                return Err(Error::value("modulo by zero"));
            }
            Value::Float(to_f64(l) % rhs)
        }
        (BinaryOp::Pow, l, r) if numeric(&l) && numeric(&r) => {
            Value::Float(to_f64(l).powf(to_f64(r)))
        }
        // String concat
        (BinaryOp::Concat, Value::String(a), Value::String(b)) => Value::String(a + &b),
        (BinaryOp::Concat, l, r) => {
            // Coerce to string and concat
            let ls = l.coerce(crate::sql::ast::DataType::String)?;
            let rs = r.coerce(crate::sql::ast::DataType::String)?;
            match (ls, rs) {
                (Value::String(a), Value::String(b)) => Value::String(a + &b),
                _ => return Err(Error::ty("|| requires string operands")),
            }
        }
        (op, l, r) => {
            return Err(Error::ty(format!(
                "operator `{}` does not apply to {} and {}",
                op.symbol(),
                l.type_name(),
                r.type_name()
            )));
        }
    })
}

fn apply_comparison(op: BinaryOp, l: Value, r: Value) -> Result<Value> {
    let cmp = match l.partial_cmp_sql(&r)? {
        None => return Ok(Value::Null),
        Some(c) => c,
    };
    use std::cmp::Ordering::*;
    let v = match op {
        BinaryOp::Eq => cmp == Equal,
        BinaryOp::NotEq => cmp != Equal,
        BinaryOp::Lt => cmp == Less,
        BinaryOp::LtEq => cmp != Greater,
        BinaryOp::Gt => cmp == Greater,
        BinaryOp::GtEq => cmp != Less,
        _ => unreachable!("non-comparison op routed to apply_comparison"),
    };
    Ok(Value::Boolean(v))
}

// SQL three-valued AND: TRUE AND NULL = NULL; FALSE AND NULL = FALSE;
// FALSE AND _ = FALSE.
fn apply_and(l: Value, r: Value) -> Result<Value> {
    let left = logical_operand("AND", &l)?;
    let right = logical_operand("AND", &r)?;
    Ok(match (left, right) {
        (Some(false), _) | (_, Some(false)) => Value::Boolean(false),
        (Some(true), Some(true)) => Value::Boolean(true),
        (None, _) | (_, None) => Value::Null,
    })
}

fn apply_or(l: Value, r: Value) -> Result<Value> {
    let left = logical_operand("OR", &l)?;
    let right = logical_operand("OR", &r)?;
    Ok(match (left, right) {
        (Some(true), _) | (_, Some(true)) => Value::Boolean(true),
        (Some(false), Some(false)) => Value::Boolean(false),
        (None, _) | (_, None) => Value::Null,
    })
}

fn logical_operand(op: &str, value: &Value) -> Result<Option<bool>> {
    match value {
        Value::Boolean(b) => Ok(Some(*b)),
        Value::Null => Ok(None),
        other => Err(Error::ty(format!(
            "{op} requires boolean operands, got {}",
            other.type_name()
        ))),
    }
}

pub(crate) fn validate_short_circuit_boolean_operand<R: Resolver + ?Sized>(
    expr: &Expression,
    r: &R,
    op: &str,
) -> Result<()> {
    match infer_expr_kind(expr, r)? {
        ExprKind::Boolean | ExprKind::Null | ExprKind::Unknown => Ok(()),
        other => Err(Error::ty(format!(
            "{op} requires boolean operands, got {}",
            other.type_name()
        ))),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExprKind {
    Null,
    Boolean,
    Integer,
    Float,
    String,
    Unknown,
}

impl ExprKind {
    fn from_value(value: Value) -> Self {
        match value {
            Value::Null => ExprKind::Null,
            Value::Boolean(_) => ExprKind::Boolean,
            Value::Integer(_) => ExprKind::Integer,
            Value::Float(_) => ExprKind::Float,
            Value::String(_) => ExprKind::String,
        }
    }

    fn type_name(self) -> &'static str {
        match self {
            ExprKind::Null => "NULL",
            ExprKind::Boolean => "BOOLEAN",
            ExprKind::Integer => "INTEGER",
            ExprKind::Float => "FLOAT",
            ExprKind::String => "STRING",
            ExprKind::Unknown => "UNKNOWN",
        }
    }

    fn is_numeric(self) -> bool {
        matches!(self, ExprKind::Integer | ExprKind::Float)
    }
}

fn infer_expr_kind<R: Resolver + ?Sized>(expr: &Expression, r: &R) -> Result<ExprKind> {
    Ok(match expr {
        Expression::Literal(lit) => ExprKind::from_value(literal_value(lit)),
        Expression::Column(name) => ExprKind::from_value(r.column(name)?),
        Expression::Qualified(table, name) => ExprKind::from_value(r.qualified(table, name)?),
        Expression::Wildcard => return Err(Error::internal("`*` cannot appear here")),
        Expression::Unary(UnaryOp::Not, inner) => {
            validate_short_circuit_boolean_operand(inner, r, "NOT")?;
            ExprKind::Boolean
        }
        Expression::Unary(UnaryOp::Plus | UnaryOp::Minus, inner) => {
            let kind = infer_expr_kind(inner, r)?;
            match kind {
                ExprKind::Null | ExprKind::Unknown => ExprKind::Unknown,
                ExprKind::Integer | ExprKind::Float => kind,
                other => {
                    return Err(Error::ty(format!(
                        "cannot apply numeric unary operator to {}",
                        other.type_name()
                    )));
                }
            }
        }
        Expression::Binary(left, BinaryOp::And, right) => {
            validate_short_circuit_boolean_operand(left, r, "AND")?;
            validate_short_circuit_boolean_operand(right, r, "AND")?;
            ExprKind::Boolean
        }
        Expression::Binary(left, BinaryOp::Or, right) => {
            validate_short_circuit_boolean_operand(left, r, "OR")?;
            validate_short_circuit_boolean_operand(right, r, "OR")?;
            ExprKind::Boolean
        }
        Expression::Binary(
            left,
            BinaryOp::Eq
            | BinaryOp::NotEq
            | BinaryOp::Lt
            | BinaryOp::LtEq
            | BinaryOp::Gt
            | BinaryOp::GtEq,
            right,
        ) => {
            let left_kind = infer_expr_kind(left, r)?;
            let right_kind = infer_expr_kind(right, r)?;
            if !comparison_kinds_compatible(left_kind, right_kind) {
                return Err(Error::ty(format!(
                    "cannot compare {} with {}",
                    left_kind.type_name(),
                    right_kind.type_name()
                )));
            }
            ExprKind::Boolean
        }
        Expression::Binary(left, BinaryOp::Concat, right) => {
            infer_expr_kind(left, r)?;
            infer_expr_kind(right, r)?;
            ExprKind::String
        }
        Expression::Binary(left, _, right) => {
            let left_kind = infer_expr_kind(left, r)?;
            let right_kind = infer_expr_kind(right, r)?;
            match (left_kind, right_kind) {
                (ExprKind::Null, _) | (_, ExprKind::Null) => ExprKind::Null,
                (l, r) if l.is_numeric() && r.is_numeric() => {
                    if l == ExprKind::Float || r == ExprKind::Float {
                        ExprKind::Float
                    } else {
                        ExprKind::Integer
                    }
                }
                (ExprKind::Unknown, _) | (_, ExprKind::Unknown) => ExprKind::Unknown,
                (l, r) => {
                    return Err(Error::ty(format!(
                        "operator does not apply to {} and {}",
                        l.type_name(),
                        r.type_name()
                    )));
                }
            }
        }
        Expression::IsNull { expr, .. } => {
            infer_expr_kind(expr, r)?;
            ExprKind::Boolean
        }
        Expression::InList { expr, list, .. } => {
            infer_expr_kind(expr, r)?;
            for item in list {
                infer_expr_kind(item, r)?;
            }
            ExprKind::Boolean
        }
        Expression::Between {
            expr, low, high, ..
        } => {
            infer_expr_kind(expr, r)?;
            infer_expr_kind(low, r)?;
            infer_expr_kind(high, r)?;
            ExprKind::Boolean
        }
        Expression::Like { expr, pattern, .. } => {
            infer_expr_kind(expr, r)?;
            infer_expr_kind(pattern, r)?;
            ExprKind::Boolean
        }
        Expression::Function { args, .. } => {
            for arg in args {
                infer_expr_kind(arg, r)?;
            }
            ExprKind::Unknown
        }
        Expression::Scalar(_) => {
            return Err(Error::internal(
                "scalar subqueries must be resolved before eval (executor bug)",
            ));
        }
        Expression::Case {
            operand,
            branches,
            otherwise,
        } => {
            if let Some(op) = operand {
                infer_expr_kind(op, r)?;
            }
            for (when, then) in branches {
                if operand.is_none() {
                    validate_short_circuit_boolean_operand(when, r, "CASE WHEN")?;
                } else {
                    infer_expr_kind(when, r)?;
                }
                infer_expr_kind(then, r)?;
            }
            if let Some(otherwise) = otherwise {
                infer_expr_kind(otherwise, r)?;
            }
            ExprKind::Unknown
        }
    })
}

fn comparison_kinds_compatible(left: ExprKind, right: ExprKind) -> bool {
    matches!(left, ExprKind::Null | ExprKind::Unknown)
        || matches!(right, ExprKind::Null | ExprKind::Unknown)
        || left == right
        || (left.is_numeric() && right.is_numeric())
}

fn numeric(v: &Value) -> bool {
    matches!(v, Value::Integer(_) | Value::Float(_))
}

fn to_f64(v: Value) -> f64 {
    match v {
        Value::Integer(n) => n as f64,
        Value::Float(f) => f,
        _ => f64::NAN,
    }
}

// ---------------------------------------------------------------------
// Built-in scalar functions
// ---------------------------------------------------------------------

fn apply_function<R: Resolver + ?Sized>(name: &str, args: &[Expression], r: &R) -> Result<Value> {
    let upper = name.to_ascii_uppercase();
    // Aggregate names get rejected here so the planner notices any that
    // the executor missed.
    if matches!(upper.as_str(), "COUNT" | "SUM" | "AVG" | "MIN" | "MAX") {
        return Err(Error::internal(format!(
            "aggregate `{name}` reached scalar evaluator"
        )));
    }
    if upper == "COALESCE" {
        for arg in args {
            let value = eval_with(arg, r)?;
            if !value.is_null() {
                return Ok(value);
            }
        }
        return Ok(Value::Null);
    }
    // Most builtins propagate NULL — evaluate args once and short-circuit.
    let evaled: Vec<Value> = args
        .iter()
        .map(|a| eval_with(a, r))
        .collect::<Result<_>>()?;
    match upper.as_str() {
        "ABS" => abs_numeric(&evaled),
        "ROUND" => unary_float(&evaled, f64::round),
        "FLOOR" => unary_float(&evaled, f64::floor),
        "CEILING" | "CEIL" => unary_float(&evaled, f64::ceil),
        "SQRT" => unary_float(&evaled, f64::sqrt),
        "LENGTH" => {
            check_arity(name, 1, &evaled)?;
            match &evaled[0] {
                Value::Null => Ok(Value::Null),
                Value::String(s) => Ok(Value::Integer(s.chars().count() as i64)),
                other => Err(Error::ty(format!(
                    "LENGTH expects string, got {}",
                    other.type_name()
                ))),
            }
        }
        "LOWER" => string_unary(name, &evaled, |s| s.to_lowercase()),
        "UPPER" => string_unary(name, &evaled, |s| s.to_uppercase()),
        "TRIM" => string_unary(name, &evaled, |s| s.trim().to_string()),
        "LTRIM" => string_unary(name, &evaled, |s| s.trim_start().to_string()),
        "RTRIM" => string_unary(name, &evaled, |s| s.trim_end().to_string()),
        "SIN" => unary_float(&evaled, f64::sin),
        "COS" => unary_float(&evaled, f64::cos),
        "TAN" => unary_float(&evaled, f64::tan),
        "EXP" => unary_float(&evaled, f64::exp),
        "LN" => unary_float(&evaled, f64::ln),
        "LOG" | "LOG10" => unary_float(&evaled, f64::log10),
        "POWER" | "POW" => {
            check_arity(name, 2, &evaled)?;
            match (&evaled[0], &evaled[1]) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (a, b) if numeric(a) && numeric(b) => {
                    Ok(Value::Float(to_f64(a.clone()).powf(to_f64(b.clone()))))
                }
                _ => Err(Error::ty("POWER expects two numeric arguments")),
            }
        }
        "MOD" => {
            check_arity(name, 2, &evaled)?;
            match (&evaled[0], &evaled[1]) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Integer(a), Value::Integer(b)) if *b != 0 => Ok(Value::Integer(
                    a.checked_rem(*b)
                        .ok_or_else(|| Error::value("integer modulo overflow"))?,
                )),
                (Value::Integer(_), Value::Integer(_)) => Err(Error::value("MOD by zero")),
                (a, b) if numeric(a) && numeric(b) => {
                    let rhs = to_f64(b.clone());
                    if rhs == 0.0 {
                        return Err(Error::value("MOD by zero"));
                    }
                    Ok(Value::Float(to_f64(a.clone()) % rhs))
                }
                _ => Err(Error::ty("MOD expects two numeric arguments")),
            }
        }
        "REVERSE" => string_unary(name, &evaled, |s| s.chars().rev().collect()),
        "REPEAT" => {
            check_arity(name, 2, &evaled)?;
            match (&evaled[0], &evaled[1]) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::String(s), Value::Integer(n)) if *n >= 0 => {
                    Ok(Value::String(s.repeat(*n as usize)))
                }
                _ => Err(Error::ty("REPEAT(string, non-negative-int)")),
            }
        }
        "REPLACE" => {
            check_arity(name, 3, &evaled)?;
            match (&evaled[0], &evaled[1], &evaled[2]) {
                (Value::Null, _, _) | (_, Value::Null, _) | (_, _, Value::Null) => Ok(Value::Null),
                (Value::String(s), Value::String(from), Value::String(to)) => {
                    Ok(Value::String(s.replace(from.as_str(), to)))
                }
                _ => Err(Error::ty("REPLACE expects three string arguments")),
            }
        }
        "SUBSTRING" | "SUBSTR" => substring(&evaled),
        "CONCAT" => {
            let mut out = String::new();
            for v in &evaled {
                if v.is_null() {
                    return Ok(Value::Null);
                }
                out.push_str(&v.to_string());
            }
            Ok(Value::String(out))
        }
        "NULLIF" => {
            check_arity(name, 2, &evaled)?;
            match evaled[0].equal_sql(&evaled[1])? {
                Some(true) => Ok(Value::Null),
                _ => Ok(evaled[0].clone()),
            }
        }
        // `IF` is a reserved keyword (used by `CREATE TABLE IF NOT EXISTS`),
        // so we expose this builtin as `IFF` instead.
        "IFF" => {
            check_arity(name, 3, &evaled)?;
            match &evaled[0] {
                Value::Boolean(true) => Ok(evaled[1].clone()),
                Value::Boolean(false) | Value::Null => Ok(evaled[2].clone()),
                other => Err(Error::ty(format!(
                    "IFF expects boolean condition, got {}",
                    other.type_name()
                ))),
            }
        }
        _ => Err(Error::ty(format!("unknown function `{name}`"))),
    }
}

fn check_arity(name: &str, want: usize, vs: &[Value]) -> Result<()> {
    if vs.len() != want {
        return Err(Error::ty(format!(
            "{name} takes {want} argument{}, got {}",
            if want == 1 { "" } else { "s" },
            vs.len()
        )));
    }
    Ok(())
}

fn abs_numeric(vs: &[Value]) -> Result<Value> {
    check_arity("ABS", 1, vs)?;
    Ok(match &vs[0] {
        Value::Null => Value::Null,
        Value::Integer(n) => Value::Integer(
            n.checked_abs()
                .ok_or_else(|| Error::value("integer abs overflow"))?,
        ),
        Value::Float(x) => Value::Float(x.abs()),
        other => {
            return Err(Error::ty(format!(
                "expected numeric, got {}",
                other.type_name()
            )));
        }
    })
}

fn unary_float(vs: &[Value], f: impl Fn(f64) -> f64) -> Result<Value> {
    check_arity("function", 1, vs)?;
    Ok(match &vs[0] {
        Value::Null => Value::Null,
        Value::Integer(n) => Value::Float(f(*n as f64)),
        Value::Float(x) => Value::Float(f(*x)),
        other => {
            return Err(Error::ty(format!(
                "expected numeric, got {}",
                other.type_name()
            )));
        }
    })
}

fn string_unary(name: &str, vs: &[Value], f: impl Fn(&str) -> String) -> Result<Value> {
    check_arity(name, 1, vs)?;
    Ok(match &vs[0] {
        Value::Null => Value::Null,
        Value::String(s) => Value::String(f(s)),
        other => {
            return Err(Error::ty(format!(
                "{name} expects string, got {}",
                other.type_name()
            )));
        }
    })
}

fn substring(vs: &[Value]) -> Result<Value> {
    if vs.len() != 2 && vs.len() != 3 {
        return Err(Error::ty("SUBSTRING takes 2 or 3 arguments"));
    }
    let s = match &vs[0] {
        Value::Null => return Ok(Value::Null),
        Value::String(s) => s.clone(),
        other => {
            return Err(Error::ty(format!(
                "SUBSTRING expects string, got {}",
                other.type_name()
            )));
        }
    };
    let start = match &vs[1] {
        Value::Null => return Ok(Value::Null),
        Value::Integer(n) => *n,
        other => {
            return Err(Error::ty(format!(
                "SUBSTRING start must be integer, got {}",
                other.type_name()
            )));
        }
    };
    // SQL is 1-based for positive starts. Non-positive starts open the
    // extraction window before the string and only return the overlap.
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len() as i64;
    let start_idx = if start > 0 {
        start.saturating_sub(1).min(len)
    } else {
        0
    } as usize;
    let end_idx = if vs.len() == 3 {
        let take = match &vs[2] {
            Value::Null => return Ok(Value::Null),
            Value::Integer(n) => *n,
            other => {
                return Err(Error::ty(format!(
                    "SUBSTRING length must be integer, got {}",
                    other.type_name()
                )));
            }
        };
        if take < 0 {
            return Err(Error::value("SUBSTRING length must be non-negative"));
        }
        let end = if start > 0 {
            start.saturating_sub(1).saturating_add(take)
        } else {
            start.saturating_add(take)
        };
        end.max(0).min(len) as usize
    } else {
        chars.len()
    };
    Ok(Value::String(chars[start_idx..end_idx].iter().collect()))
}

// ---------------------------------------------------------------------
// LIKE pattern matching
// ---------------------------------------------------------------------

/// Re-export of `like_match` for use by sibling executor modules — the
/// inner helper is intentionally not `pub`, but the aggregate evaluator
/// needs structural access to mirror LIKE semantics inside grouped
/// projections.
pub(crate) fn like_match_for_test(haystack: &str, pattern: &str) -> bool {
    like_match(haystack, pattern)
}

/// SQL `LIKE`: `_` matches any single character, `%` matches zero or
/// more. Escaping is not supported (we'd need an `ESCAPE` clause).
fn like_match(haystack: &str, pattern: &str) -> bool {
    let h: Vec<char> = haystack.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    let mut dp = vec![vec![false; p.len() + 1]; h.len() + 1];
    dp[0][0] = true;
    for j in 1..=p.len() {
        if p[j - 1] == '%' {
            dp[0][j] = dp[0][j - 1];
        }
    }
    for i in 1..=h.len() {
        for j in 1..=p.len() {
            dp[i][j] = match p[j - 1] {
                '%' => dp[i][j - 1] || dp[i - 1][j],
                '_' => dp[i - 1][j - 1],
                c => h[i - 1] == c && dp[i - 1][j - 1],
            };
        }
    }
    dp[h.len()][p.len()]
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::Parser;

    fn ev(expr: &str) -> Value {
        let mut p = Parser::new(expr).unwrap();
        let e = p.parse_expression().unwrap();
        eval(&e).unwrap_or_else(|err| panic!("eval err in `{expr}`: {err}"))
    }

    fn ev_err(expr: &str) -> String {
        let mut p = Parser::new(expr).unwrap();
        let e = p.parse_expression().unwrap();
        eval(&e).unwrap_err().to_string()
    }

    // -------- arithmetic ------------------------------------------------

    #[test]
    fn arith_int() {
        assert_eq!(ev("1 + 2 * 3"), Value::Integer(7));
        assert_eq!(ev("10 / 3"), Value::Integer(3));
        assert_eq!(ev("10 % 3"), Value::Integer(1));
        assert_eq!(ev("2 ^ 10"), Value::Integer(1024));
    }

    #[test]
    fn arith_float() {
        assert_eq!(ev("1.0 + 2.0"), Value::Float(3.0));
        assert_eq!(ev("10.0 / 4.0"), Value::Float(2.5));
    }

    #[test]
    fn arith_promotes_int_to_float() {
        assert_eq!(ev("1 + 0.5"), Value::Float(1.5));
        assert_eq!(ev("3 * 2.0"), Value::Float(6.0));
    }

    #[test]
    fn arith_div_by_zero() {
        assert!(ev_err("1 / 0").contains("division by zero"));
        assert!(ev_err("1.0 / 0.0").contains("division by zero"));
        assert!(ev_err("1.0 % 0.0").contains("modulo by zero"));
        assert!(ev_err("2 ^ -2147483649").contains("exponent out of range"));
    }

    #[test]
    fn integer_overflow_errors() {
        assert!(ev_err("9223372036854775807 + 1").contains("overflow"));
        assert!(ev_err("9223372036854775807 * 2").contains("overflow"));
    }

    #[test]
    fn unary_minus() {
        assert_eq!(ev("-5"), Value::Integer(-5));
        assert_eq!(ev("- - 5"), Value::Integer(5));
    }

    #[test]
    fn arith_with_null() {
        assert_eq!(ev("NULL + 1"), Value::Null);
        assert_eq!(ev("1 + NULL"), Value::Null);
    }

    // -------- comparison / logic ---------------------------------------

    #[test]
    fn comparison_int() {
        assert_eq!(ev("1 < 2"), Value::Boolean(true));
        assert_eq!(ev("1 = 1"), Value::Boolean(true));
        assert_eq!(ev("1 <> 2"), Value::Boolean(true));
    }

    #[test]
    fn comparison_with_null() {
        assert_eq!(ev("1 = NULL"), Value::Null);
        assert_eq!(ev("NULL <> NULL"), Value::Null);
    }

    #[test]
    fn logic_three_valued() {
        assert_eq!(ev("TRUE AND FALSE"), Value::Boolean(false));
        assert_eq!(ev("TRUE OR FALSE"), Value::Boolean(true));
        assert_eq!(ev("NOT TRUE"), Value::Boolean(false));
        assert_eq!(ev("TRUE AND NULL"), Value::Null);
        assert_eq!(ev("FALSE AND NULL"), Value::Boolean(false));
        assert_eq!(ev("TRUE OR NULL"), Value::Boolean(true));
        assert_eq!(ev("FALSE OR NULL"), Value::Null);
        assert_eq!(ev("NOT NULL"), Value::Null);
        assert!(ev_err("NULL AND 1").contains("requires boolean"));
        assert!(ev_err("NULL OR 1").contains("requires boolean"));
    }

    // -------- string ---------------------------------------------------

    #[test]
    fn string_concat() {
        assert_eq!(ev("'a' || 'b'"), Value::String("ab".into()));
        // Coercion: int || str
        assert_eq!(ev("1 || 'x'"), Value::String("1x".into()));
    }

    #[test]
    fn string_funcs() {
        assert_eq!(ev("LOWER('AbC')"), Value::String("abc".into()));
        assert_eq!(ev("UPPER('abc')"), Value::String("ABC".into()));
        assert_eq!(ev("TRIM('  hi  ')"), Value::String("hi".into()));
        assert_eq!(ev("LENGTH('abc')"), Value::Integer(3));
        assert_eq!(ev("LENGTH('café')"), Value::Integer(4));
    }

    #[test]
    fn substring_works() {
        assert_eq!(ev("SUBSTRING('hello', 2, 3)"), Value::String("ell".into()));
        assert_eq!(ev("SUBSTRING('hello', 2)"), Value::String("ello".into()));
        // 1-based, beyond bounds
        assert_eq!(ev("SUBSTRING('abc', 1, 100)"), Value::String("abc".into()));
        assert_eq!(ev("SUBSTRING('abc', 5)"), Value::String("".into()));
        assert_eq!(
            ev("SUBSTRING('abc', 2, 9223372036854775807)"),
            Value::String("bc".into())
        );
        assert_eq!(ev("SUBSTRING('abcdef', -3, 4)"), Value::String("a".into()));
        assert!(ev_err("SUBSTRING('abcdef', 1, -1)").contains("non-negative"));
    }

    #[test]
    fn concat_function() {
        assert_eq!(ev("CONCAT('a', 'b', 'c')"), Value::String("abc".into()));
        assert_eq!(ev("CONCAT('a', NULL)"), Value::Null);
    }

    // -------- IS NULL / IN / BETWEEN / LIKE ----------------------------

    #[test]
    fn is_null_works() {
        assert_eq!(ev("NULL IS NULL"), Value::Boolean(true));
        assert_eq!(ev("NULL IS NOT NULL"), Value::Boolean(false));
        assert_eq!(ev("1 IS NULL"), Value::Boolean(false));
    }

    #[test]
    fn in_list_works() {
        assert_eq!(ev("1 IN (1, 2, 3)"), Value::Boolean(true));
        assert_eq!(ev("4 IN (1, 2, 3)"), Value::Boolean(false));
        assert_eq!(ev("4 NOT IN (1, 2, 3)"), Value::Boolean(true));
        // NULL semantics
        assert_eq!(ev("NULL IN (1)"), Value::Null);
        assert_eq!(ev("1 IN (NULL)"), Value::Null);
        assert_eq!(ev("1 IN (1, NULL)"), Value::Boolean(true));
    }

    #[test]
    fn between_works() {
        assert_eq!(ev("5 BETWEEN 1 AND 10"), Value::Boolean(true));
        assert_eq!(ev("0 BETWEEN 1 AND 10"), Value::Boolean(false));
        assert_eq!(ev("10 BETWEEN 1 AND 10"), Value::Boolean(true)); // inclusive
        assert_eq!(ev("5 NOT BETWEEN 1 AND 10"), Value::Boolean(false));
    }

    #[test]
    fn like_match_basic() {
        assert_eq!(ev("'hello' LIKE 'h%'"), Value::Boolean(true));
        assert_eq!(ev("'hello' LIKE '%lo'"), Value::Boolean(true));
        assert_eq!(ev("'hello' LIKE 'h_llo'"), Value::Boolean(true));
        assert_eq!(ev("'hello' LIKE 'world'"), Value::Boolean(false));
        assert_eq!(ev("'abc' NOT LIKE 'a%'"), Value::Boolean(false));
        assert_eq!(
            ev("'aaaaaaaaab' LIKE '%a%a%a%a%a%a%a%a%b'"),
            Value::Boolean(true)
        );
    }

    #[test]
    fn like_with_null() {
        assert_eq!(ev("NULL LIKE 'a%'"), Value::Null);
        assert_eq!(ev("'a' LIKE NULL"), Value::Null);
    }

    // -------- functions ------------------------------------------------

    #[test]
    fn abs_function() {
        assert_eq!(ev("ABS(-5)"), Value::Integer(5));
        assert_eq!(ev("ABS(-5.5)"), Value::Float(5.5));
        assert!(ev_err("ABS(-9223372036854775807 - 1)").contains("overflow"));
    }

    #[test]
    fn coalesce_function() {
        assert_eq!(ev("COALESCE(NULL, NULL, 3)"), Value::Integer(3));
        assert_eq!(ev("COALESCE(NULL, NULL, NULL)"), Value::Null);
        assert_eq!(ev("COALESCE(1, 2)"), Value::Integer(1));
        assert_eq!(ev("COALESCE(1, 1 / 0)"), Value::Integer(1));
    }

    #[test]
    fn nullif_function() {
        assert_eq!(ev("NULLIF(1, 1)"), Value::Null);
        assert_eq!(ev("NULLIF(1, 2)"), Value::Integer(1));
    }

    #[test]
    fn reverse_repeat_replace() {
        assert_eq!(ev("REVERSE('abc')"), Value::String("cba".into()));
        assert_eq!(ev("REPEAT('ab', 3)"), Value::String("ababab".into()));
        assert_eq!(
            ev("REPLACE('hello', 'l', 'L')"),
            Value::String("heLLo".into())
        );
        assert_eq!(ev("REPEAT('x', 0)"), Value::String("".into()));
    }

    #[test]
    fn trim_variants() {
        assert_eq!(ev("LTRIM('  abc  ')"), Value::String("abc  ".into()));
        assert_eq!(ev("RTRIM('  abc  ')"), Value::String("  abc".into()));
        assert_eq!(ev("TRIM('  abc  ')"), Value::String("abc".into()));
    }

    #[test]
    fn math_functions() {
        match ev("SIN(0)") {
            Value::Float(f) => assert!(f.abs() < 1e-9),
            _ => panic!(),
        }
        match ev("COS(0)") {
            Value::Float(f) => assert!((f - 1.0).abs() < 1e-9),
            _ => panic!(),
        }
        match ev("LN(1)") {
            Value::Float(f) => assert!(f.abs() < 1e-9),
            _ => panic!(),
        }
        match ev("POWER(2, 8)") {
            Value::Float(f) => assert_eq!(f, 256.0),
            _ => panic!(),
        }
        assert_eq!(ev("MOD(10, 3)"), Value::Integer(1));
        assert!(ev_err("MOD(-9223372036854775807 - 1, -1)").contains("overflow"));
        assert!(ev_err("MOD(1.0, 0.0)").contains("MOD by zero"));
    }

    #[test]
    fn iff_function() {
        assert_eq!(ev("IFF(TRUE, 1, 2)"), Value::Integer(1));
        assert_eq!(ev("IFF(FALSE, 1, 2)"), Value::Integer(2));
        assert_eq!(ev("IFF(NULL, 1, 2)"), Value::Integer(2));
    }

    #[test]
    fn floor_ceiling_round() {
        assert_eq!(ev("FLOOR(3.7)"), Value::Float(3.0));
        assert_eq!(ev("CEIL(3.2)"), Value::Float(4.0));
        assert_eq!(ev("ROUND(3.5)"), Value::Float(4.0));
    }

    // -------- column resolution ----------------------------------------

    struct OneCol(&'static str, Value);
    impl Resolver for OneCol {
        fn column(&self, name: &str) -> Result<Value> {
            if name == self.0 {
                Ok(self.1.clone())
            } else {
                Err(Error::ty(format!("unknown column `{name}`")))
            }
        }
        fn qualified(&self, _: &str, _: &str) -> Result<Value> {
            Err(Error::ty("no qualified"))
        }
    }

    #[test]
    fn column_resolution() {
        let mut p = Parser::new("a + 10").unwrap();
        let e = p.parse_expression().unwrap();
        let r = OneCol("a", Value::Integer(5));
        assert_eq!(eval_with(&e, &r).unwrap(), Value::Integer(15));
    }

    // -------- CASE -----------------------------------------------------

    #[test]
    fn case_boolean_form() {
        // CASE WHEN cond THEN ... ELSE ...
        assert_eq!(ev("CASE WHEN TRUE THEN 1 ELSE 2 END"), Value::Integer(1));
        assert_eq!(ev("CASE WHEN FALSE THEN 1 ELSE 2 END"), Value::Integer(2));
        assert_eq!(ev("CASE WHEN NULL THEN 1 ELSE 2 END"), Value::Integer(2));
    }

    #[test]
    fn case_no_else_returns_null() {
        assert_eq!(ev("CASE WHEN FALSE THEN 1 END"), Value::Null);
    }

    #[test]
    fn case_switch_form() {
        // CASE expr WHEN val THEN ...
        assert_eq!(
            ev("CASE 2 WHEN 1 THEN 'a' WHEN 2 THEN 'b' ELSE 'c' END"),
            Value::String("b".into())
        );
        assert_eq!(ev("CASE 'x' WHEN 'y' THEN 1 ELSE 2 END"), Value::Integer(2));
    }

    #[test]
    fn case_chained_conditions() {
        // First matching WHEN wins.
        let e = "CASE WHEN 1 = 2 THEN 'a' WHEN 1 < 2 THEN 'b' WHEN TRUE THEN 'c' END";
        assert_eq!(ev(e), Value::String("b".into()));
    }

    #[test]
    fn unknown_function_errors() {
        let mut p = Parser::new("frobnicate(1)").unwrap();
        let e = p.parse_expression().unwrap();
        let err = eval(&e).unwrap_err();
        assert!(err.to_string().contains("unknown function"));
    }
}
