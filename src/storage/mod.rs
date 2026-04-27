//! Persistent storage primitives.
//!
//! - [`encoding`] — hand-rolled binary serialiser for `Value`, `Row`,
//!   table schemas. We avoid `serde` to keep the on-disk layout obvious
//!   and easy to evolve.
//! - [`page`] — fixed-size 8 KiB slotted pages.
//! - [`pager`] — file-backed paged storage with a small LRU cache.
//! - [`wal`] — write-ahead log for crash recovery.

pub mod encoding;
pub mod page;
pub mod pager;
pub mod wal;

pub use encoding::{decode_row, decode_table, decode_value, encode_row, encode_table, encode_value};
pub use page::{Page, PageId, PageType, PAGE_SIZE};
pub use pager::Pager;
pub use wal::{LogRecord, Wal};
