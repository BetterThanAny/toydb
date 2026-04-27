//! Query planning and execution.
//!
//! `Executor` is constructed around a `&mut dyn Engine` and dispatches
//! a parsed [`Statement`] to the appropriate operator. Operators are
//! intentionally non-generic: the goal is correctness and readability,
//! not throughput.

pub mod expr;
pub mod plan;
pub mod result;

pub use expr::{eval, eval_with, Resolver};
pub use plan::Executor;
pub use result::{Column as ResultColumn, ResultSet};
