//! Storage engines.
//!
//! Every backend implements [`Engine`]. The executor never reaches
//! around the trait — that way swapping the in-memory backend for a
//! disk-backed one is a one-line change in `main`.

pub mod disk;
pub mod memory;

pub use disk::DiskEngine;
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
    fn scan(&mut self, table: &str) -> Result<Vec<(RowId, Row)>>;
    /// Replace the row at `id`. The returned `Result<()>` does **not**
    /// guarantee the RowId stays valid: a backend that has to relocate
    /// the row (because the new bytes don't fit in the original slot)
    /// is allowed to invalidate `id` and emit a fresh row internally.
    /// Callers that need stable identity should use the engine's
    /// constraint mechanisms (e.g. PRIMARY KEY) instead of holding
    /// RowIds across `update` calls.
    fn update(&mut self, table: &str, id: RowId, row: Row) -> Result<()>;
    fn delete(&mut self, table: &str, id: RowId) -> Result<()>;
    fn get(&mut self, table: &str, id: RowId) -> Result<Option<Row>>;

    // -- Transactions --------------------------------------------------
    //
    // Default implementations decline transactions outright. Engines
    // that do support them (e.g. [`MemoryEngine`]) override these.

    /// Begin a transaction. Subsequent writes are buffered until
    /// [`commit`] or [`rollback`].
    fn begin(&mut self) -> Result<()> {
        Err(crate::error::Error::other(
            "this engine does not support transactions",
        ))
    }

    /// Commit the in-progress transaction. No-op if no transaction.
    fn commit(&mut self) -> Result<()> {
        Err(crate::error::Error::other(
            "this engine does not support transactions",
        ))
    }

    /// Roll back the in-progress transaction.
    fn rollback(&mut self) -> Result<()> {
        Err(crate::error::Error::other(
            "this engine does not support transactions",
        ))
    }

    /// Whether a transaction is currently in progress.
    fn in_transaction(&self) -> bool { false }

    /// Append a new column to an existing table. Default implementation
    /// declines; engines override to support `ALTER TABLE`.
    fn add_column(&mut self, _table: &str, _column: crate::catalog::Column) -> Result<()> {
        Err(crate::error::Error::other(
            "this engine does not support ALTER TABLE ADD COLUMN",
        ))
    }
}
