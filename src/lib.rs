//! toydb — a from-scratch SQL database engine.
//!
//! Top-level layout:
//! - [`sql`]      — lexer, parser, AST
//! - [`types`]    — value / data-type / row primitives
//! - [`catalog`]  — table and column metadata
//! - [`engine`]   — pluggable storage engines (memory, disk)
//! - [`executor`] — query planning and execution
//! - [`storage`]  — low-level page / pager / B-tree / WAL
//! - [`txn`]      — MVCC transaction layer
//!
//! See `README.md` for an overview and `PLAN.md` for milestones.

pub mod catalog;
pub mod engine;
pub mod error;
pub mod executor;
pub mod format;
pub mod sql;
pub mod storage;
pub mod txn;
pub mod types;

pub use error::{Error, Result};
pub use executor::{Executor, ResultSet};
pub use sql::Parser;
