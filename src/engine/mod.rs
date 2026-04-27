//! Storage engines.
//!
//! Every backend implements [`Engine`]. The executor never reaches
//! around the trait — that way swapping the in-memory backend for a
//! disk-backed one is a one-line change in `main`.

pub mod memory;

pub use memory::MemoryEngine;

use crate::catalog::Table;
use crate::error::Result;
use crate::types::row::Row;

/// Synthetic row identifier used for UPDATE/DELETE targeting. Memory
/// and disk backends both keep this stable for the life of a row.
pub type RowId = u64;

/// Pluggable storage backend. The trait is intentionally small: every
/// extra method is an extra page-layout decision the disk backend has
/// to honour.
pub trait Engine {
    // -- DDL -----------------------------------------------------------
    fn create_table(&mut self, table: Table) -> Result<()>;
    fn drop_table(&mut self, name: &str, if_exists: bool) -> Result<bool>;
    fn get_table(&self, name: &str) -> Result<&Table>;
    fn list_tables(&self) -> Vec<String>;

    // -- DML -----------------------------------------------------------
    fn insert(&mut self, table: &str, row: Row) -> Result<RowId>;
    fn scan(&self, table: &str) -> Result<Vec<(RowId, Row)>>;
    fn update(&mut self, table: &str, id: RowId, row: Row) -> Result<()>;
    fn delete(&mut self, table: &str, id: RowId) -> Result<()>;
    fn get(&self, table: &str, id: RowId) -> Result<Option<Row>>;
}
