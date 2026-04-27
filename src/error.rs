//! Crate-wide error type.
//!
//! Every fallible API in toydb returns [`Result<T>`]. Errors carry
//! enough context (positions, identifiers) for diagnostics — the parser
//! and lexer attach line/column data to syntax errors.

use std::io;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    /// Lexer rejected input.
    #[error("lex error at line {line}, col {col}: {msg}")]
    Lex { line: usize, col: usize, msg: String },

    /// Parser rejected token stream.
    #[error("parse error at line {line}, col {col}: {msg}")]
    Parse { line: usize, col: usize, msg: String },

    /// Schema / catalog inconsistency: missing table, duplicate column, ...
    #[error("schema error: {0}")]
    Schema(String),

    /// Type mismatch detected during expression evaluation or insert.
    #[error("type error: {0}")]
    Type(String),

    /// A value violates a column constraint (NOT NULL, PRIMARY KEY, ...).
    #[error("constraint violation: {0}")]
    Constraint(String),

    /// Caller supplied a value that cannot be coerced to the requested type.
    #[error("value error: {0}")]
    Value(String),

    /// Storage backend reported an I/O failure.
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    /// Internal invariant broken — bug in toydb, never user-triggered.
    #[error("internal error: {0}")]
    Internal(String),

    /// Transaction aborted (write/write conflict, deadlock, ...).
    #[error("transaction aborted: {0}")]
    Aborted(String),

    /// Catch-all for messages that don't yet have a richer variant.
    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn lex(line: usize, col: usize, msg: impl Into<String>) -> Self {
        Self::Lex { line, col, msg: msg.into() }
    }

    pub fn parse(line: usize, col: usize, msg: impl Into<String>) -> Self {
        Self::Parse { line, col, msg: msg.into() }
    }

    pub fn schema(msg: impl Into<String>) -> Self {
        Self::Schema(msg.into())
    }

    pub fn ty(msg: impl Into<String>) -> Self {
        Self::Type(msg.into())
    }

    pub fn constraint(msg: impl Into<String>) -> Self {
        Self::Constraint(msg.into())
    }

    pub fn value(msg: impl Into<String>) -> Self {
        Self::Value(msg.into())
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }

    pub fn aborted(msg: impl Into<String>) -> Self {
        Self::Aborted(msg.into())
    }

    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
