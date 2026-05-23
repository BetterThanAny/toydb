//! Table and column metadata.
//!
//! A [`Table`] is the schema half of a relation: name plus ordered
//! columns plus the index of the primary key (if any). Storage engines
//! hold the rows; the catalog holds the [`Table`] descriptor.

use crate::error::{Error, Result};
use crate::sql::ast::{ColumnDef, DataType, Expression};
use crate::types::value::Value;

/// Column definition in a table schema.
///
/// `default` is stored as an [`Expression`] rather than a [`Value`] so
/// that we can support defaults like `current_timestamp` later. For now
/// the executor folds it to a value at insert time.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub name: String,
    pub ty: DataType,
    pub primary_key: bool,
    pub nullable: bool,
    pub unique: bool,
    pub default: Option<Expression>,
}

impl Column {
    pub fn new(name: impl Into<String>, ty: DataType) -> Self {
        Self {
            name: name.into(),
            ty,
            primary_key: false,
            nullable: true,
            unique: false,
            default: None,
        }
    }

    pub fn primary_key(mut self) -> Self {
        self.primary_key = true;
        self.nullable = false;
        self.unique = true;
        self
    }

    pub fn not_null(mut self) -> Self {
        self.nullable = false;
        self
    }
    pub fn unique(mut self) -> Self {
        self.unique = true;
        self
    }
    pub fn nullable(mut self, n: bool) -> Self {
        self.nullable = n;
        self
    }
    pub fn default_value(mut self, e: Expression) -> Self {
        self.default = Some(e);
        self
    }

    /// Validate a value against this column's constraints. Coerces it
    /// into the column's declared type (so `INTEGER` columns happily
    /// accept `1.0`). NOT NULL is enforced after coercion.
    pub fn validate(&self, value: Value) -> Result<Value> {
        if value.is_null() {
            if !self.nullable {
                return Err(Error::constraint(format!(
                    "column `{}` is NOT NULL",
                    self.name
                )));
            }
            return Ok(Value::Null);
        }
        let coerced = value.coerce(self.ty)?;
        Ok(coerced)
    }
}

impl From<&ColumnDef> for Column {
    fn from(def: &ColumnDef) -> Self {
        Self {
            name: def.name.clone(),
            ty: def.ty,
            primary_key: def.primary_key,
            nullable: def.nullable && !def.primary_key,
            unique: def.unique || def.primary_key,
            default: def.default.clone(),
        }
    }
}

/// Single-column secondary index metadata.
///
/// The index contents live in the storage engine. The catalog persists
/// the name/table/column descriptor so disk-backed engines can rebuild
/// the runtime tree after opening a database or replaying the WAL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    pub name: String,
    pub table: String,
    pub column: String,
}

impl Index {
    pub fn new(
        name: impl Into<String>,
        table: impl Into<String>,
        column: impl Into<String>,
    ) -> Result<Self> {
        let name = name.into();
        let table = table.into();
        let column = column.into();
        if name.is_empty() {
            return Err(Error::schema("index name must not be empty"));
        }
        if table.is_empty() {
            return Err(Error::schema("index table must not be empty"));
        }
        if column.is_empty() {
            return Err(Error::schema("index column must not be empty"));
        }
        Ok(Self {
            name,
            table,
            column,
        })
    }
}

/// A table schema: name, columns, and the index of the primary-key
/// column (if any). At most one PK per table.
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    pub primary_key: Option<usize>,
    pub indexes: Vec<Index>,
}

impl Table {
    pub fn new(name: impl Into<String>, mut columns: Vec<Column>) -> Result<Self> {
        let name = name.into();
        if name.is_empty() {
            return Err(Error::schema("table name must not be empty"));
        }
        if columns.is_empty() {
            return Err(Error::schema(format!(
                "table `{name}` must declare at least one column"
            )));
        }
        // Reject duplicate column names (case-sensitive).
        for i in 0..columns.len() {
            for j in (i + 1)..columns.len() {
                if columns[i].name == columns[j].name {
                    return Err(Error::schema(format!(
                        "duplicate column `{}` in table `{name}`",
                        columns[i].name
                    )));
                }
            }
        }
        for col in &mut columns {
            if col.primary_key {
                col.nullable = false;
                col.unique = true;
            }
        }
        let pks: Vec<usize> = columns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.primary_key)
            .map(|(i, _)| i)
            .collect();
        if pks.len() > 1 {
            return Err(Error::schema(format!(
                "table `{name}` has more than one PRIMARY KEY"
            )));
        }
        let primary_key = pks.into_iter().next();
        Ok(Self {
            name,
            columns,
            primary_key,
            indexes: Vec::new(),
        })
    }

    /// Look up a column index by name. Case-sensitive.
    pub fn column_index(&self, name: &str) -> Result<usize> {
        self.columns
            .iter()
            .position(|c| c.name == name)
            .ok_or_else(|| Error::schema(format!("table `{}` has no column `{name}`", self.name)))
    }

    /// Borrow the primary-key column descriptor, if any.
    pub fn primary_key_column(&self) -> Option<&Column> {
        self.primary_key.map(|i| &self.columns[i])
    }

    pub fn add_index(&mut self, index: Index) -> Result<()> {
        if index.table != self.name {
            return Err(Error::schema(format!(
                "index `{}` targets table `{}`, not `{}`",
                index.name, index.table, self.name
            )));
        }
        self.column_index(&index.column)?;
        if self.indexes.iter().any(|i| i.name == index.name) {
            return Err(Error::schema(format!(
                "index `{}` already exists on table `{}`",
                index.name, self.name
            )));
        }
        self.indexes.push(index);
        Ok(())
    }

    pub fn drop_index(&mut self, name: &str) -> Option<Index> {
        let pos = self.indexes.iter().position(|i| i.name == name)?;
        Some(self.indexes.remove(pos))
    }

    pub fn index_on_column(&self, column: &str) -> Option<&Index> {
        self.indexes.iter().find(|i| i.column == column)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::ast::{Expression, Literal};

    fn col(n: &str, ty: DataType) -> Column {
        Column::new(n, ty)
    }

    #[test]
    fn build_simple_table() {
        let t = Table::new(
            "users",
            vec![
                col("id", DataType::Integer).primary_key(),
                col("name", DataType::String).not_null(),
            ],
        )
        .unwrap();
        assert_eq!(t.primary_key, Some(0));
        assert_eq!(t.column_index("name").unwrap(), 1);
        assert!(t.column_index("missing").is_err());
    }

    #[test]
    fn rejects_empty_columns() {
        assert!(Table::new("t", vec![]).is_err());
    }

    #[test]
    fn rejects_duplicate_columns() {
        let r = Table::new(
            "t",
            vec![col("a", DataType::Integer), col("a", DataType::String)],
        );
        assert!(r.unwrap_err().to_string().contains("duplicate"));
    }

    #[test]
    fn rejects_multiple_primary_keys() {
        let r = Table::new(
            "t",
            vec![
                col("a", DataType::Integer).primary_key(),
                col("b", DataType::Integer).primary_key(),
            ],
        );
        assert!(
            r.unwrap_err()
                .to_string()
                .contains("more than one PRIMARY KEY")
        );
    }

    #[test]
    fn validate_null_against_not_null() {
        let c = col("a", DataType::Integer).not_null();
        assert!(c.validate(Value::Null).is_err());
        assert!(c.validate(Value::Integer(1)).is_ok());
    }

    #[test]
    fn validate_coerces_int_to_float() {
        let c = col("a", DataType::Float);
        let v = c.validate(Value::Integer(3)).unwrap();
        assert_eq!(v, Value::Float(3.0));
    }

    #[test]
    fn from_column_def_propagates_unique_for_pk() {
        let def = ColumnDef {
            name: "id".into(),
            ty: DataType::Integer,
            primary_key: true,
            nullable: false,
            unique: false,
            default: Some(Expression::Literal(Literal::Integer(0))),
        };
        let c = Column::from(&def);
        assert!(c.unique);
        assert!(c.primary_key);
    }

    #[test]
    fn add_and_drop_index_metadata() {
        let mut t = Table::new(
            "users",
            vec![
                col("id", DataType::Integer).primary_key(),
                col("age", DataType::Integer),
            ],
        )
        .unwrap();
        t.add_index(Index::new("idx_users_age", "users", "age").unwrap())
            .unwrap();
        assert_eq!(t.index_on_column("age").unwrap().name, "idx_users_age");
        assert!(
            t.add_index(Index::new("idx_bad", "users", "missing").unwrap())
                .is_err()
        );
        assert_eq!(t.drop_index("idx_users_age").unwrap().column, "age");
        assert!(t.indexes.is_empty());
    }
}
