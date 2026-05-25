//! Runtime values.
//!
//! [`Value`] is the cell type — a tagged union of all the things a
//! column can hold. Comparison and arithmetic follow SQL three-valued
//! logic where NULL participates: `NULL op anything == NULL`.

use std::cmp::Ordering;
use std::fmt;

use crate::error::{Error, Result};
use crate::sql::ast::DataType;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Boolean(bool),
    Integer(i64),
    Float(f64),
    String(String),
}

impl Value {
    /// Logical type of this value. NULL has no type, so we return
    /// `None` for it — callers either pick the column's declared type
    /// or surface a "type unknowable" error.
    pub fn datatype(&self) -> Option<DataType> {
        match self {
            Value::Null => None,
            Value::Boolean(_) => Some(DataType::Boolean),
            Value::Integer(_) => Some(DataType::Integer),
            Value::Float(_) => Some(DataType::Float),
            Value::String(_) => Some(DataType::String),
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Try to coerce into a target [`DataType`], following the project's
    /// implicit-conversion rules:
    ///
    /// - NULL stays NULL regardless of target
    /// - Integer ↔ Float convert numerically
    /// - Integer / Float / Bool can serialise to String
    /// - everything else is a [`Error::Type`]
    pub fn coerce(&self, target: DataType) -> Result<Value> {
        Ok(match (self, target) {
            (Value::Null, _) => Value::Null,
            (Value::Boolean(b), DataType::Boolean) => Value::Boolean(*b),
            (Value::Boolean(_), DataType::Integer) => {
                return Err(Error::ty("cannot convert BOOLEAN to INTEGER"));
            }
            (Value::Boolean(_), DataType::Float) => {
                return Err(Error::ty("cannot convert BOOLEAN to FLOAT"));
            }
            (Value::Boolean(b), DataType::String) => Value::String(b.to_string()),
            (Value::Integer(n), DataType::Integer) => Value::Integer(*n),
            (Value::Integer(n), DataType::Float) => Value::Float(*n as f64),
            (Value::Integer(n), DataType::String) => Value::String(n.to_string()),
            (Value::Integer(_), DataType::Boolean) => {
                return Err(Error::ty("cannot convert INTEGER to BOOLEAN"));
            }
            (Value::Float(f), DataType::Float) => Value::Float(*f),
            (Value::Float(f), DataType::Integer) => {
                if !f.is_finite() {
                    return Err(Error::ty(format!(
                        "cannot convert non-finite float {f} to integer"
                    )));
                }
                if f.fract() != 0.0 {
                    return Err(Error::ty(format!(
                        "cannot convert non-integral float {f} to INTEGER"
                    )));
                }
                if *f < i64::MIN as f64 || *f >= 9223372036854775808.0_f64 {
                    return Err(Error::ty(format!("float {f} is out of range for INTEGER")));
                }
                Value::Integer(*f as i64)
            }
            (Value::Float(f), DataType::String) => Value::String(format_float(*f)),
            (Value::Float(_), DataType::Boolean) => {
                return Err(Error::ty("cannot convert FLOAT to BOOLEAN"));
            }
            (Value::String(s), DataType::String) => Value::String(s.clone()),
            (Value::String(s), DataType::Integer) => Value::Integer(
                s.parse()
                    .map_err(|_| Error::ty(format!("cannot parse `{s}` as INTEGER")))?,
            ),
            (Value::String(s), DataType::Float) => Value::Float(
                s.parse()
                    .map_err(|_| Error::ty(format!("cannot parse `{s}` as FLOAT")))?,
            ),
            (Value::String(s), DataType::Boolean) => match s.to_ascii_lowercase().as_str() {
                "true" => Value::Boolean(true),
                "false" => Value::Boolean(false),
                _ => return Err(Error::ty(format!("cannot parse `{s}` as BOOLEAN"))),
            },
        })
    }

    /// Three-valued ordering: returns `None` if either operand is NULL,
    /// otherwise an [`Ordering`]. Cross-type comparisons promote integer
    /// to float when needed; otherwise the operands must match.
    pub fn partial_cmp_sql(&self, other: &Value) -> Result<Option<Ordering>> {
        use Value::*;
        Ok(match (self, other) {
            (Null, _) | (_, Null) => None,
            (Boolean(a), Boolean(b)) => Some(a.cmp(b)),
            (Integer(a), Integer(b)) => Some(a.cmp(b)),
            (Integer(a), Float(b)) => (*a as f64).partial_cmp(b),
            (Float(a), Integer(b)) => a.partial_cmp(&(*b as f64)),
            (Float(a), Float(b)) => a.partial_cmp(b),
            (String(a), String(b)) => Some(a.cmp(b)),
            (a, b) => {
                return Err(Error::ty(format!(
                    "cannot compare {} with {}",
                    a.type_name(),
                    b.type_name()
                )));
            }
        })
    }

    /// SQL three-valued equality: `NULL = anything` is `Unknown` (None).
    pub fn equal_sql(&self, other: &Value) -> Result<Option<bool>> {
        match self.partial_cmp_sql(other)? {
            None => Ok(None),
            Some(o) => Ok(Some(o == Ordering::Equal)),
        }
    }

    /// Total ordering for use in sort/index keys. NULL sorts *first*
    /// (smaller than everything), and incompatible types fall back to
    /// the variant tag — total ordering takes priority over SQL semantics
    /// here because we need a usable `Ord`.
    pub fn total_cmp(&self, other: &Value) -> Ordering {
        use Value::*;
        match (self, other) {
            (Null, Null) => Ordering::Equal,
            (Null, _) => Ordering::Less,
            (_, Null) => Ordering::Greater,
            (Boolean(a), Boolean(b)) => a.cmp(b),
            (Integer(a), Integer(b)) => a.cmp(b),
            (Integer(a), Float(b)) => (*a as f64).total_cmp(b),
            (Float(a), Integer(b)) => a.total_cmp(&(*b as f64)),
            (Float(a), Float(b)) => a.total_cmp(b),
            (String(a), String(b)) => a.cmp(b),
            (a, b) => a.tag().cmp(&b.tag()),
        }
    }

    fn tag(&self) -> u8 {
        match self {
            Value::Null => 0,
            Value::Boolean(_) => 1,
            Value::Integer(_) => 2,
            Value::Float(_) => 3,
            Value::String(_) => 4,
        }
    }

    /// Human-readable name for error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "NULL",
            Value::Boolean(_) => "BOOLEAN",
            Value::Integer(_) => "INTEGER",
            Value::Float(_) => "FLOAT",
            Value::String(_) => "STRING",
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("NULL"),
            Value::Boolean(b) => f.write_str(if *b { "TRUE" } else { "FALSE" }),
            Value::Integer(n) => write!(f, "{n}"),
            Value::Float(x) => f.write_str(&format_float(*x)),
            Value::String(s) => f.write_str(s),
        }
    }
}

/// Render a `f64` as SQL would — finite numbers use Rust's default
/// formatting (which is round-trip safe), and non-finite are spelled out.
fn format_float(f: f64) -> String {
    if f.is_nan() {
        "NaN".into()
    } else if f.is_infinite() {
        if f.is_sign_positive() {
            "Inf".into()
        } else {
            "-Inf".into()
        }
    } else if f == f.trunc() && f.abs() < 1e16 {
        // Integers we display as `1.0` rather than `1` so the type
        // still reads as float in REPL output.
        format!("{f:.1}")
    } else {
        format!("{f}")
    }
}

// Convenience constructors used pervasively in tests and the executor.
impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Integer(v)
    }
}
impl From<i32> for Value {
    fn from(v: i32) -> Self {
        Value::Integer(v as i64)
    }
}
impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Float(v)
    }
}
impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Boolean(v)
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::String(v.into())
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::String(v)
    }
}
impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        match v {
            Some(x) => x.into(),
            None => Value::Null,
        }
    }
}

// ----------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coerce_null_passthrough() {
        assert_eq!(Value::Null.coerce(DataType::Integer).unwrap(), Value::Null);
        assert_eq!(Value::Null.coerce(DataType::String).unwrap(), Value::Null);
    }

    #[test]
    fn coerce_int_to_float() {
        assert_eq!(
            Value::Integer(3).coerce(DataType::Float).unwrap(),
            Value::Float(3.0)
        );
    }

    #[test]
    fn coerce_float_to_int_requires_integral_value() {
        assert_eq!(
            Value::Float(3.0).coerce(DataType::Integer).unwrap(),
            Value::Integer(3)
        );
        assert!(Value::Float(3.9).coerce(DataType::Integer).is_err());
        assert!(Value::Float(1e20).coerce(DataType::Integer).is_err());
        assert!(
            Value::Float(9223372036854775808.0)
                .coerce(DataType::Integer)
                .is_err()
        );
    }

    #[test]
    fn coerce_string_to_int() {
        assert_eq!(
            Value::String("42".into())
                .coerce(DataType::Integer)
                .unwrap(),
            Value::Integer(42)
        );
        assert!(
            Value::String("abc".into())
                .coerce(DataType::Integer)
                .is_err()
        );
    }

    #[test]
    fn coerce_string_to_bool() {
        assert_eq!(
            Value::String("true".into())
                .coerce(DataType::Boolean)
                .unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            Value::String("FALSE".into())
                .coerce(DataType::Boolean)
                .unwrap(),
            Value::Boolean(false)
        );
        assert!(
            Value::String("maybe".into())
                .coerce(DataType::Boolean)
                .is_err()
        );
        assert!(Value::String("1".into()).coerce(DataType::Boolean).is_err());
        assert!(Value::String("t".into()).coerce(DataType::Boolean).is_err());
    }

    #[test]
    fn coerce_nan_to_int_errors() {
        assert!(Value::Float(f64::NAN).coerce(DataType::Integer).is_err());
        assert!(
            Value::Float(f64::INFINITY)
                .coerce(DataType::Integer)
                .is_err()
        );
    }

    #[test]
    fn cmp_int_vs_float_promotes() {
        let a = Value::Integer(5);
        let b = Value::Float(5.5);
        assert_eq!(a.partial_cmp_sql(&b).unwrap(), Some(Ordering::Less));
        assert_eq!(b.partial_cmp_sql(&a).unwrap(), Some(Ordering::Greater));
    }

    #[test]
    fn cmp_with_null_returns_none() {
        assert!(
            Value::Null
                .partial_cmp_sql(&Value::Integer(0))
                .unwrap()
                .is_none()
        );
        assert!(
            Value::Integer(0)
                .partial_cmp_sql(&Value::Null)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn cmp_incompatible_errors() {
        assert!(
            Value::String("x".into())
                .partial_cmp_sql(&Value::Integer(1))
                .is_err()
        );
    }

    #[test]
    fn equal_sql_three_valued() {
        assert_eq!(
            Value::Integer(1).equal_sql(&Value::Integer(1)).unwrap(),
            Some(true)
        );
        assert_eq!(
            Value::Integer(1).equal_sql(&Value::Integer(2)).unwrap(),
            Some(false)
        );
        assert_eq!(Value::Null.equal_sql(&Value::Integer(1)).unwrap(), None);
        assert_eq!(Value::Null.equal_sql(&Value::Null).unwrap(), None);
    }

    #[test]
    fn total_cmp_null_first() {
        assert_eq!(Value::Null.total_cmp(&Value::Integer(0)), Ordering::Less);
        assert_eq!(Value::Integer(0).total_cmp(&Value::Null), Ordering::Greater);
        assert_eq!(Value::Null.total_cmp(&Value::Null), Ordering::Equal);
    }

    #[test]
    fn total_cmp_cross_type_uses_tag() {
        // String "1" is greater than Integer 1 because String tag (4) > Integer tag (2).
        assert_eq!(
            Value::String("1".into()).total_cmp(&Value::Integer(1)),
            Ordering::Greater
        );
    }

    #[test]
    fn display_renders_sql_friendly() {
        assert_eq!(Value::Null.to_string(), "NULL");
        assert_eq!(Value::Boolean(true).to_string(), "TRUE");
        assert_eq!(Value::Integer(42).to_string(), "42");
        assert_eq!(Value::Float(2.5).to_string(), "2.5");
        assert_eq!(Value::Float(2.0).to_string(), "2.0");
        assert_eq!(Value::String("hi".into()).to_string(), "hi");
    }

    #[test]
    fn from_impls_work() {
        let v: Value = 1i64.into();
        assert_eq!(v, Value::Integer(1));
        let v: Value = "abc".into();
        assert_eq!(v, Value::String("abc".into()));
        let v: Value = Some(3i64).into();
        assert_eq!(v, Value::Integer(3));
        let v: Value = None::<i64>.into();
        assert_eq!(v, Value::Null);
    }
}
