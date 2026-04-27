//! Abstract Syntax Tree for SQL statements.
//!
//! Designed for *correctness and clarity* over compactness; nodes own
//! their strings (no source-tied lifetimes) so that the AST can be passed
//! across module boundaries (planner, executor, error messages) freely.

use std::fmt;

// ---------------------------------------------------------------------
// Statements
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Select(Box<SelectStmt>),
    Insert(Box<InsertStmt>),
    Update(Box<UpdateStmt>),
    Delete(Box<DeleteStmt>),
    CreateTable(Box<CreateTableStmt>),
    DropTable(Box<DropTableStmt>),
    Begin,
    Commit,
    Rollback,
    Explain(Box<Statement>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct SelectStmt {
    pub distinct: bool,
    pub items: Vec<SelectItem>,
    pub from: Option<FromClause>,
    pub r#where: Option<Expression>,
    pub group_by: Vec<Expression>,
    pub having: Option<Expression>,
    pub order_by: Vec<OrderBy>,
    pub limit: Option<Expression>,
    pub offset: Option<Expression>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    /// `*` — every column from every source.
    Wildcard,
    /// `t.*` — every column from a single table or alias.
    QualifiedWildcard(String),
    /// Expression with an optional `AS alias`.
    Expr { expr: Expression, alias: Option<String> },
}

#[derive(Debug, Clone, PartialEq)]
pub enum FromClause {
    /// `FROM users u` or just `FROM users`.
    Table { name: String, alias: Option<String> },
    /// Any join — left and right may themselves be join sub-trees.
    Join {
        left: Box<FromClause>,
        kind: JoinKind,
        right: Box<FromClause>,
        on: Expression,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderBy {
    pub expr: Expression,
    /// `true` for `ASC` (or default), `false` for `DESC`.
    pub asc: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InsertStmt {
    pub table: String,
    /// `INSERT INTO t (a, b) VALUES (...)` — `None` means "use declared order".
    pub columns: Option<Vec<String>>,
    pub rows: Vec<Vec<Expression>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UpdateStmt {
    pub table: String,
    pub assignments: Vec<(String, Expression)>,
    pub r#where: Option<Expression>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeleteStmt {
    pub table: String,
    pub r#where: Option<Expression>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateTableStmt {
    pub name: String,
    pub if_not_exists: bool,
    pub columns: Vec<ColumnDef>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub ty: DataType,
    pub primary_key: bool,
    pub nullable: bool,
    pub unique: bool,
    pub default: Option<Expression>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    Boolean,
    Integer,
    Float,
    String,
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DataType::Boolean => f.write_str("BOOLEAN"),
            DataType::Integer => f.write_str("INTEGER"),
            DataType::Float => f.write_str("FLOAT"),
            DataType::String => f.write_str("STRING"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropTableStmt {
    pub name: String,
    pub if_exists: bool,
}

// ---------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Expression {
    Literal(Literal),
    /// Bare column reference: `name`.
    Column(String),
    /// Qualified reference: `t.name`.
    Qualified(String, String),
    Unary(UnaryOp, Box<Expression>),
    Binary(Box<Expression>, BinaryOp, Box<Expression>),
    /// `expr IS [NOT] NULL`. `negated` flips the sense.
    IsNull { expr: Box<Expression>, negated: bool },
    /// `expr [NOT] IN (a, b, c)`.
    InList { expr: Box<Expression>, list: Vec<Expression>, negated: bool },
    /// `expr [NOT] BETWEEN low AND high`.
    Between { expr: Box<Expression>, low: Box<Expression>, high: Box<Expression>, negated: bool },
    /// `expr [NOT] LIKE pattern`.
    Like { expr: Box<Expression>, pattern: Box<Expression>, negated: bool },
    /// `f(arg, arg, ...)` — function call. We also model `COUNT(*)` here
    /// using a single `Wildcard` arg. `distinct` is set for
    /// `COUNT(DISTINCT col)` and friends; ordinary calls leave it false.
    Function { name: String, args: Vec<Expression>, distinct: bool },
    /// `*` inside a function call (specifically `COUNT(*)`).
    Wildcard,
    /// `CASE [operand] WHEN ... THEN ... [ELSE ...] END`. With operand:
    /// a "switch" form comparing against the operand. Without: a chain
    /// of independent boolean conditions.
    Case {
        operand: Option<Box<Expression>>,
        branches: Vec<(Expression, Expression)>,
        otherwise: Option<Box<Expression>>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Null,
    Boolean(bool),
    Integer(i64),
    Float(f64),
    String(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Plus,
    Minus,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
    Concat,
}

impl BinaryOp {
    pub fn symbol(&self) -> &'static str {
        match self {
            BinaryOp::Add => "+",
            BinaryOp::Sub => "-",
            BinaryOp::Mul => "*",
            BinaryOp::Div => "/",
            BinaryOp::Mod => "%",
            BinaryOp::Pow => "^",
            BinaryOp::Eq => "=",
            BinaryOp::NotEq => "<>",
            BinaryOp::Lt => "<",
            BinaryOp::LtEq => "<=",
            BinaryOp::Gt => ">",
            BinaryOp::GtEq => ">=",
            BinaryOp::And => "AND",
            BinaryOp::Or => "OR",
            BinaryOp::Concat => "||",
        }
    }
}
