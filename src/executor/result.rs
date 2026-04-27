//! Result set returned by [`crate::executor::Executor::execute`].

use crate::types::row::Row;

#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub name: String,
}

impl Column {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResultSet {
    CreateTable { name: String },
    DropTable { name: String, existed: bool },
    AlterTable { name: String },
    Insert { count: usize },
    Update { count: usize },
    Delete { count: usize },
    Select { columns: Vec<Column>, rows: Vec<Row> },
    Begin,
    Commit,
    Rollback,
    Explain(String),
}

impl ResultSet {
    /// True when the result is a row set (`SELECT` / `EXPLAIN`).
    pub fn is_query(&self) -> bool {
        matches!(self, ResultSet::Select { .. } | ResultSet::Explain(_))
    }
}
