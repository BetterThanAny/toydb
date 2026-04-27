//! Value system: data types, runtime values, rows.
//!
//! toydb is intentionally narrow — five logical types (NULL, Boolean,
//! Integer, Float, String) — chosen so coercion rules stay obvious. The
//! [`Value`] enum is the single runtime currency; every layer above
//! storage operates on it.

pub mod row;
pub mod value;

pub use row::Row;
pub use value::Value;

pub use crate::sql::ast::DataType;
