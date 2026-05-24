//! SQL parser — recursive descent, hand-written.
//!
//! Top-down with a Pratt-style climbing routine for expressions. The
//! grammar handled here is a strict subset of SQL but covers everything
//! the executor will need (DDL, DML, transaction control, EXPLAIN).

use crate::error::{Error, Result};
use crate::sql::ast::*;
use crate::sql::lexer::{Lexer, Spanned, Token};

pub struct Parser {
    tokens: Vec<Spanned>,
    pos: usize,
}

impl Parser {
    pub fn new(input: &str) -> Result<Self> {
        let tokens = Lexer::collect_all(input)?;
        Ok(Self { tokens, pos: 0 })
    }

    /// Parse the next single statement. Optional trailing semicolon is
    /// consumed. Returns `Ok(None)` at end of input.
    pub fn parse_statement(&mut self) -> Result<Option<Statement>> {
        // Allow leading semicolons (`; SELECT 1` is OK).
        while matches!(self.peek_tok(), Some(Token::Semicolon)) {
            self.bump();
        }
        if self.peek_tok().is_none() {
            return Ok(None);
        }
        let stmt = self.parse_statement_inner()?;
        // Consume an optional trailing semicolon.
        if matches!(self.peek_tok(), Some(Token::Semicolon)) {
            self.bump();
        }
        Ok(Some(stmt))
    }

    /// Parse all statements in the input. Convenient for batch SQL files
    /// and tests.
    pub fn parse_all(input: &str) -> Result<Vec<Statement>> {
        let mut p = Self::new(input)?;
        let mut out = Vec::new();
        loop {
            while matches!(p.peek_tok(), Some(Token::Semicolon)) {
                p.bump();
            }
            if p.peek_tok().is_none() {
                break;
            }
            out.push(p.parse_statement_inner()?);
            if matches!(p.peek_tok(), Some(Token::Semicolon)) {
                p.bump();
            } else if let Some(extra) = p.peek().cloned() {
                return Err(Error::parse(
                    extra.line,
                    extra.col,
                    format!("expected semicolon before {}", extra.token.as_str()),
                ));
            }
        }
        Ok(out)
    }

    /// Convenience helper: parse a single complete statement and reject
    /// trailing input.
    pub fn parse_one(input: &str) -> Result<Statement> {
        let mut p = Self::new(input)?;
        let stmt = p
            .parse_statement()?
            .ok_or_else(|| Error::parse(1, 1, "empty input"))?;
        if let Some(extra) = p.peek().cloned() {
            return Err(Error::parse(
                extra.line,
                extra.col,
                format!("unexpected trailing token {}", extra.token.as_str()),
            ));
        }
        Ok(stmt)
    }

    // ------------------------------------------------------------------
    // Statement dispatcher
    // ------------------------------------------------------------------

    fn parse_statement_inner(&mut self) -> Result<Statement> {
        let t = self
            .peek()
            .cloned()
            .ok_or_else(|| Error::parse(0, 0, "unexpected EOF"))?;
        match &t.token {
            Token::KwSelect => self.parse_select().map(|s| Statement::Select(Box::new(s))),
            Token::KwInsert => self.parse_insert().map(|s| Statement::Insert(Box::new(s))),
            Token::KwUpdate => self.parse_update().map(|s| Statement::Update(Box::new(s))),
            Token::KwDelete => self.parse_delete().map(|s| Statement::Delete(Box::new(s))),
            Token::KwCreate if matches!(self.peek_tok_at(1), Some(Token::KwIndex)) => self
                .parse_create_index()
                .map(|s| Statement::CreateIndex(Box::new(s))),
            Token::KwCreate => self
                .parse_create_table()
                .map(|s| Statement::CreateTable(Box::new(s))),
            Token::KwDrop if matches!(self.peek_tok_at(1), Some(Token::KwIndex)) => self
                .parse_drop_index()
                .map(|s| Statement::DropIndex(Box::new(s))),
            Token::KwDrop => self
                .parse_drop_table()
                .map(|s| Statement::DropTable(Box::new(s))),
            Token::KwAlter => self
                .parse_alter_table()
                .map(|s| Statement::AlterTable(Box::new(s))),
            Token::KwBegin => {
                self.bump();
                // Optional `TRANSACTION` keyword.
                if matches!(self.peek_tok(), Some(Token::KwTransaction)) {
                    self.bump();
                }
                Ok(Statement::Begin)
            }
            Token::KwCommit => {
                self.bump();
                if matches!(self.peek_tok(), Some(Token::KwTransaction)) {
                    self.bump();
                }
                Ok(Statement::Commit)
            }
            Token::KwRollback => {
                self.bump();
                if matches!(self.peek_tok(), Some(Token::KwTransaction)) {
                    self.bump();
                }
                Ok(Statement::Rollback)
            }
            Token::KwExplain => {
                self.bump();
                let inner = self.parse_statement_inner()?;
                Ok(Statement::Explain(Box::new(inner)))
            }
            _ => Err(self.err_here(format!(
                "unexpected start of statement: {}",
                t.token.as_str()
            ))),
        }
    }

    // ------------------------------------------------------------------
    // SELECT
    // ------------------------------------------------------------------

    fn parse_select(&mut self) -> Result<SelectStmt> {
        // Parse the first SELECT *core* (no ORDER BY / LIMIT / OFFSET yet).
        let mut head = self.parse_select_core()?;
        // Stitch together any UNION clauses.
        let mut unions = Vec::new();
        while self.consume_if(&Token::KwUnion) {
            let all = self.consume_if(&Token::KwAll);
            let next = self.parse_select_core()?;
            unions.push(UnionPart {
                all,
                query: Box::new(next),
            });
        }
        // ORDER BY / LIMIT / OFFSET apply to the *combined* result.
        let (order_by, limit, offset) = self.parse_order_limit_offset()?;
        head.order_by = order_by;
        head.limit = limit;
        head.offset = offset;
        head.unions = unions;
        Ok(head)
    }

    /// Parse one SELECT without trailing ORDER BY / LIMIT / OFFSET.
    fn parse_select_core(&mut self) -> Result<SelectStmt> {
        self.expect(Token::KwSelect)?;
        let distinct = self.consume_if(&Token::KwDistinct);

        let mut items = Vec::new();
        loop {
            items.push(self.parse_select_item()?);
            if !self.consume_if(&Token::Comma) {
                break;
            }
        }

        let from = if self.consume_if(&Token::KwFrom) {
            Some(self.parse_from_clause()?)
        } else {
            None
        };

        let r#where = if self.consume_if(&Token::KwWhere) {
            Some(self.parse_expression()?)
        } else {
            None
        };

        let mut group_by = Vec::new();
        if self.consume_if(&Token::KwGroup) {
            self.expect(Token::KwBy)?;
            loop {
                group_by.push(self.parse_expression()?);
                if !self.consume_if(&Token::Comma) {
                    break;
                }
            }
        }

        let having = if self.consume_if(&Token::KwHaving) {
            Some(self.parse_expression()?)
        } else {
            None
        };

        Ok(SelectStmt {
            distinct,
            items,
            from,
            r#where,
            group_by,
            having,
            order_by: Vec::new(),
            limit: None,
            offset: None,
            unions: Vec::new(),
        })
    }

    fn parse_order_limit_offset(
        &mut self,
    ) -> Result<(Vec<OrderBy>, Option<Expression>, Option<Expression>)> {
        let mut order_by = Vec::new();
        if self.consume_if(&Token::KwOrder) {
            self.expect(Token::KwBy)?;
            loop {
                let expr = self.parse_expression()?;
                let asc = match self.peek_tok() {
                    Some(Token::KwAsc) => {
                        self.bump();
                        true
                    }
                    Some(Token::KwDesc) => {
                        self.bump();
                        false
                    }
                    _ => true,
                };
                let nulls_first = if self.consume_if(&Token::KwNulls) {
                    match self.peek_tok() {
                        Some(Token::KwFirst) => {
                            self.bump();
                            true
                        }
                        Some(Token::KwLast) => {
                            self.bump();
                            false
                        }
                        _ => return Err(self.err_here("expected NULLS FIRST or NULLS LAST")),
                    }
                } else {
                    !asc
                };
                order_by.push(OrderBy {
                    expr,
                    asc,
                    nulls_first,
                });
                if !self.consume_if(&Token::Comma) {
                    break;
                }
            }
        }
        let limit = if self.consume_if(&Token::KwLimit) {
            Some(self.parse_expression()?)
        } else {
            None
        };
        let offset = if self.consume_if(&Token::KwOffset) {
            Some(self.parse_expression()?)
        } else {
            None
        };
        Ok((order_by, limit, offset))
    }

    fn parse_select_item(&mut self) -> Result<SelectItem> {
        // Bare `*`
        if matches!(self.peek_tok(), Some(Token::Star)) {
            self.bump();
            return Ok(SelectItem::Wildcard);
        }
        // `ident.*` qualified wildcard — we have to peek two tokens.
        if let (Some(Token::Ident(name)), Some(Token::Dot)) = (self.peek_tok(), self.peek_tok_at(1))
        {
            // Star three tokens deep?
            if matches!(self.peek_tok_at(2), Some(Token::Star)) {
                let alias = name.clone();
                self.bump(); // ident
                self.bump(); // dot
                self.bump(); // star
                return Ok(SelectItem::QualifiedWildcard(alias));
            }
        }
        let expr = self.parse_expression()?;
        let alias = self.parse_optional_alias()?;
        Ok(SelectItem::Expr { expr, alias })
    }

    fn parse_optional_alias(&mut self) -> Result<Option<String>> {
        if self.consume_if(&Token::KwAs) {
            let name = self.expect_ident("alias")?;
            Ok(Some(name))
        } else if let Some(Token::Ident(_)) = self.peek_tok() {
            // Bare alias (no AS) — but only if the next token would not
            // continue the expression. We stop alias inference if the
            // following identifier is followed by something that clearly
            // starts the next clause... actually, simplest rule:
            // a bare identifier here is always an alias because we already
            // consumed a complete expression.
            let name = self.expect_ident("alias")?;
            Ok(Some(name))
        } else {
            Ok(None)
        }
    }

    fn parse_from_clause(&mut self) -> Result<FromClause> {
        let mut left = self.parse_from_atom()?;
        loop {
            let kind = match self.peek_tok() {
                Some(Token::KwInner) => {
                    self.bump();
                    self.expect(Token::KwJoin)?;
                    JoinKind::Inner
                }
                Some(Token::KwLeft) => {
                    self.bump();
                    let _ = self.consume_if(&Token::KwOuter);
                    self.expect(Token::KwJoin)?;
                    JoinKind::Left
                }
                Some(Token::KwRight) => {
                    self.bump();
                    let _ = self.consume_if(&Token::KwOuter);
                    self.expect(Token::KwJoin)?;
                    JoinKind::Right
                }
                Some(Token::KwJoin) => {
                    self.bump();
                    JoinKind::Inner
                }
                _ => break,
            };
            let right = self.parse_from_atom()?;
            self.expect(Token::KwOn)?;
            let on = self.parse_expression()?;
            left = FromClause::Join {
                left: Box::new(left),
                kind,
                right: Box::new(right),
                on,
            };
        }
        Ok(left)
    }

    fn parse_from_atom(&mut self) -> Result<FromClause> {
        let name = self.expect_ident("table name")?;
        let alias = match self.peek_tok() {
            Some(Token::KwAs) => {
                self.bump();
                Some(self.expect_ident("alias")?)
            }
            Some(Token::Ident(_)) => Some(self.expect_ident("alias")?),
            _ => None,
        };
        Ok(FromClause::Table { name, alias })
    }

    // ------------------------------------------------------------------
    // INSERT
    // ------------------------------------------------------------------

    fn parse_insert(&mut self) -> Result<InsertStmt> {
        self.expect(Token::KwInsert)?;
        self.expect(Token::KwInto)?;
        let table = self.expect_ident("table name")?;
        let columns = if self.consume_if(&Token::LParen) {
            let mut cols = Vec::new();
            loop {
                cols.push(self.expect_ident("column name")?);
                if !self.consume_if(&Token::Comma) {
                    break;
                }
            }
            self.expect(Token::RParen)?;
            Some(cols)
        } else {
            None
        };
        // Two forms: `VALUES (...)` or `SELECT ...` (copy).
        let source = match self.peek_tok() {
            Some(Token::KwSelect) => {
                let s = self.parse_select()?;
                InsertSource::Select(Box::new(s))
            }
            Some(Token::KwValues) => {
                self.bump();
                let mut rows = Vec::new();
                loop {
                    self.expect(Token::LParen)?;
                    let mut row = Vec::new();
                    loop {
                        row.push(self.parse_expression()?);
                        if !self.consume_if(&Token::Comma) {
                            break;
                        }
                    }
                    self.expect(Token::RParen)?;
                    rows.push(row);
                    if !self.consume_if(&Token::Comma) {
                        break;
                    }
                }
                InsertSource::Values(rows)
            }
            _ => return Err(self.err_here("expected VALUES or SELECT after INSERT INTO ...")),
        };
        Ok(InsertStmt {
            table,
            columns,
            source,
        })
    }

    // ------------------------------------------------------------------
    // UPDATE
    // ------------------------------------------------------------------

    fn parse_update(&mut self) -> Result<UpdateStmt> {
        self.expect(Token::KwUpdate)?;
        let table = self.expect_ident("table name")?;
        self.expect(Token::KwSet)?;
        let mut assignments = Vec::new();
        loop {
            let col = self.expect_ident("column name")?;
            self.expect(Token::Eq)?;
            let value = self.parse_expression()?;
            assignments.push((col, value));
            if !self.consume_if(&Token::Comma) {
                break;
            }
        }
        let r#where = if self.consume_if(&Token::KwWhere) {
            Some(self.parse_expression()?)
        } else {
            None
        };
        Ok(UpdateStmt {
            table,
            assignments,
            r#where,
        })
    }

    // ------------------------------------------------------------------
    // DELETE
    // ------------------------------------------------------------------

    fn parse_delete(&mut self) -> Result<DeleteStmt> {
        self.expect(Token::KwDelete)?;
        self.expect(Token::KwFrom)?;
        let table = self.expect_ident("table name")?;
        let r#where = if self.consume_if(&Token::KwWhere) {
            Some(self.parse_expression()?)
        } else {
            None
        };
        Ok(DeleteStmt { table, r#where })
    }

    // ------------------------------------------------------------------
    // CREATE TABLE / DROP TABLE
    // ------------------------------------------------------------------

    fn parse_create_table(&mut self) -> Result<CreateTableStmt> {
        self.expect(Token::KwCreate)?;
        self.expect(Token::KwTable)?;
        let if_not_exists = if self.consume_if(&Token::KwIf) {
            self.expect(Token::KwNot)?;
            self.expect(Token::KwExists)?;
            true
        } else {
            false
        };
        let name = self.expect_ident("table name")?;
        self.expect(Token::LParen)?;
        let mut columns = Vec::new();
        loop {
            columns.push(self.parse_column_def()?);
            if !self.consume_if(&Token::Comma) {
                break;
            }
        }
        self.expect(Token::RParen)?;
        Ok(CreateTableStmt {
            name,
            if_not_exists,
            columns,
        })
    }

    fn parse_column_def(&mut self) -> Result<ColumnDef> {
        let name = self.expect_ident("column name")?;
        let ty = self.parse_data_type()?;
        let mut primary_key = false;
        let mut nullable = true;
        let mut nullable_set = false;
        let mut unique = false;
        let mut default = None;
        loop {
            match self.peek_tok() {
                Some(Token::KwPrimary) => {
                    self.bump();
                    self.expect(Token::KwKey)?;
                    primary_key = true;
                    if !nullable_set {
                        nullable = false;
                    }
                }
                Some(Token::KwNot) => {
                    self.bump();
                    self.expect(Token::KwNull)?;
                    nullable = false;
                    nullable_set = true;
                }
                Some(Token::KwNull) => {
                    self.bump();
                    nullable = true;
                    nullable_set = true;
                }
                Some(Token::KwUnique) => {
                    self.bump();
                    unique = true;
                }
                Some(Token::KwDefault) => {
                    self.bump();
                    default = Some(self.parse_expression()?);
                }
                _ => break,
            }
        }
        Ok(ColumnDef {
            name,
            ty,
            primary_key,
            nullable,
            unique,
            default,
        })
    }

    fn parse_data_type(&mut self) -> Result<DataType> {
        let t = self
            .peek()
            .cloned()
            .ok_or_else(|| self.err_here("expected data type"))?;
        let ty = match &t.token {
            Token::KwBoolean | Token::KwBool => DataType::Boolean,
            Token::KwInteger | Token::KwInt => DataType::Integer,
            Token::KwFloat | Token::KwDouble => DataType::Float,
            Token::KwText | Token::KwString | Token::KwVarchar | Token::KwChar => DataType::String,
            other => {
                return Err(Error::parse(
                    t.line,
                    t.col,
                    format!("expected data type, found {}", other.as_str()),
                ));
            }
        };
        self.bump();
        // Optional length spec — `VARCHAR(255)` parses but length is ignored.
        if self.consume_if(&Token::LParen) {
            // skip until matching )
            let mut depth = 1;
            while depth > 0 {
                match self.bump() {
                    Some(s) => match s.token {
                        Token::LParen => depth += 1,
                        Token::RParen => depth -= 1,
                        _ => {}
                    },
                    None => return Err(self.err_here("unterminated `(` in data type")),
                }
            }
        }
        Ok(ty)
    }

    fn parse_alter_table(&mut self) -> Result<AlterTableStmt> {
        self.expect(Token::KwAlter)?;
        self.expect(Token::KwTable)?;
        let name = self.expect_ident("table name")?;
        self.expect(Token::KwAdd)?;
        // `COLUMN` is optional ergonomics, like Postgres.
        let _ = self.consume_if(&Token::KwColumn);
        let column = self.parse_column_def()?;
        Ok(AlterTableStmt {
            name,
            action: AlterAction::AddColumn(column),
        })
    }

    fn parse_drop_table(&mut self) -> Result<DropTableStmt> {
        self.expect(Token::KwDrop)?;
        self.expect(Token::KwTable)?;
        let if_exists = if self.consume_if(&Token::KwIf) {
            self.expect(Token::KwExists)?;
            true
        } else {
            false
        };
        let name = self.expect_ident("table name")?;
        Ok(DropTableStmt { name, if_exists })
    }

    fn parse_create_index(&mut self) -> Result<CreateIndexStmt> {
        self.expect(Token::KwCreate)?;
        self.expect(Token::KwIndex)?;
        let name = self.expect_ident("index name")?;
        self.expect(Token::KwOn)?;
        let table = self.expect_ident("table name")?;
        self.expect(Token::LParen)?;
        let column = self.expect_ident("column name")?;
        self.expect(Token::RParen)?;
        Ok(CreateIndexStmt {
            name,
            table,
            column,
        })
    }

    fn parse_drop_index(&mut self) -> Result<DropIndexStmt> {
        self.expect(Token::KwDrop)?;
        self.expect(Token::KwIndex)?;
        let name = self.expect_ident("index name")?;
        Ok(DropIndexStmt { name })
    }

    // ------------------------------------------------------------------
    // Expressions — Pratt parser
    //
    // Precedence ladder (low → high):
    //   1. OR
    //   2. AND
    //   3. NOT
    //   4. comparison (=, <>, <, <=, >, >=, IS NULL, IN, BETWEEN, LIKE)
    //   5. concat (||)
    //   6. additive (+, -)
    //   7. multiplicative (*, /, %)
    //   8. exponent (^)            -- right-associative
    //   9. unary prefix (+, -)
    //  10. atom
    // ------------------------------------------------------------------

    pub fn parse_expression(&mut self) -> Result<Expression> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expression> {
        let mut left = self.parse_and()?;
        while self.consume_if(&Token::KwOr) {
            let right = self.parse_and()?;
            left = Expression::Binary(Box::new(left), BinaryOp::Or, Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expression> {
        let mut left = self.parse_not()?;
        while self.consume_if(&Token::KwAnd) {
            let right = self.parse_not()?;
            left = Expression::Binary(Box::new(left), BinaryOp::And, Box::new(right));
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<Expression> {
        if self.consume_if(&Token::KwNot) {
            let inner = self.parse_not()?;
            Ok(Expression::Unary(UnaryOp::Not, Box::new(inner)))
        } else {
            self.parse_comparison()
        }
    }

    fn parse_comparison(&mut self) -> Result<Expression> {
        let left = self.parse_concat()?;
        // Single comparison operator (not chained, like SQL).
        let op = match self.peek_tok() {
            Some(Token::Eq) => Some(BinaryOp::Eq),
            Some(Token::NotEq) => Some(BinaryOp::NotEq),
            Some(Token::Lt) => Some(BinaryOp::Lt),
            Some(Token::LtEq) => Some(BinaryOp::LtEq),
            Some(Token::Gt) => Some(BinaryOp::Gt),
            Some(Token::GtEq) => Some(BinaryOp::GtEq),
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let right = self.parse_concat()?;
            return Ok(Expression::Binary(Box::new(left), op, Box::new(right)));
        }
        // IS [NOT] NULL
        if self.consume_if(&Token::KwIs) {
            let negated = self.consume_if(&Token::KwNot);
            self.expect(Token::KwNull)?;
            return Ok(Expression::IsNull {
                expr: Box::new(left),
                negated,
            });
        }
        // [NOT] IN / BETWEEN / LIKE
        let negated = self.consume_if(&Token::KwNot);
        if self.consume_if(&Token::KwIn) {
            self.expect(Token::LParen)?;
            let mut list = Vec::new();
            if !matches!(self.peek_tok(), Some(Token::RParen)) {
                loop {
                    list.push(self.parse_expression()?);
                    if !self.consume_if(&Token::Comma) {
                        break;
                    }
                }
            }
            self.expect(Token::RParen)?;
            return Ok(Expression::InList {
                expr: Box::new(left),
                list,
                negated,
            });
        }
        if self.consume_if(&Token::KwLike) {
            let pattern = self.parse_concat()?;
            return Ok(Expression::Like {
                expr: Box::new(left),
                pattern: Box::new(pattern),
                negated,
            });
        }
        if self.consume_if(&Token::KwBetween) {
            let low = self.parse_concat()?;
            self.expect(Token::KwAnd)?;
            let high = self.parse_concat()?;
            return Ok(Expression::Between {
                expr: Box::new(left),
                low: Box::new(low),
                high: Box::new(high),
                negated,
            });
        }
        if negated {
            return Err(self.err_here("expected IN, LIKE, or BETWEEN after NOT"));
        }
        Ok(left)
    }

    fn parse_concat(&mut self) -> Result<Expression> {
        let mut left = self.parse_additive()?;
        while self.consume_if(&Token::Concat) {
            let right = self.parse_additive()?;
            left = Expression::Binary(Box::new(left), BinaryOp::Concat, Box::new(right));
        }
        Ok(left)
    }

    fn parse_additive(&mut self) -> Result<Expression> {
        let mut left = self.parse_multiplicative()?;
        loop {
            let op = match self.peek_tok() {
                Some(Token::Plus) => BinaryOp::Add,
                Some(Token::Minus) => BinaryOp::Sub,
                _ => break,
            };
            self.bump();
            let right = self.parse_multiplicative()?;
            left = Expression::Binary(Box::new(left), op, Box::new(right));
        }
        Ok(left)
    }

    fn parse_multiplicative(&mut self) -> Result<Expression> {
        let mut left = self.parse_exponent()?;
        loop {
            let op = match self.peek_tok() {
                Some(Token::Star) => BinaryOp::Mul,
                Some(Token::Slash) => BinaryOp::Div,
                Some(Token::Percent) => BinaryOp::Mod,
                _ => break,
            };
            self.bump();
            let right = self.parse_exponent()?;
            left = Expression::Binary(Box::new(left), op, Box::new(right));
        }
        Ok(left)
    }

    fn parse_exponent(&mut self) -> Result<Expression> {
        let left = self.parse_unary()?;
        if self.consume_if(&Token::Caret) {
            // right-associative
            let right = self.parse_exponent()?;
            return Ok(Expression::Binary(
                Box::new(left),
                BinaryOp::Pow,
                Box::new(right),
            ));
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expression> {
        match self.peek_tok() {
            Some(Token::Plus) => {
                self.bump();
                let e = self.parse_unary()?;
                Ok(Expression::Unary(UnaryOp::Plus, Box::new(e)))
            }
            Some(Token::Minus) => {
                self.bump();
                let e = self.parse_unary()?;
                Ok(Expression::Unary(UnaryOp::Minus, Box::new(e)))
            }
            _ => self.parse_atom(),
        }
    }

    fn parse_atom(&mut self) -> Result<Expression> {
        let t = self
            .peek()
            .cloned()
            .ok_or_else(|| self.err_here("unexpected EOF in expression"))?;
        match &t.token {
            Token::KwCase => self.parse_case(),
            Token::KwNull => {
                self.bump();
                Ok(Expression::Literal(Literal::Null))
            }
            Token::KwTrue => {
                self.bump();
                Ok(Expression::Literal(Literal::Boolean(true)))
            }
            Token::KwFalse => {
                self.bump();
                Ok(Expression::Literal(Literal::Boolean(false)))
            }
            Token::Number(s) => {
                let s = s.clone();
                self.bump();
                if s.contains('.') || s.contains('e') || s.contains('E') {
                    let f: f64 = s.parse().map_err(|_| {
                        Error::parse(t.line, t.col, format!("invalid float literal `{s}`"))
                    })?;
                    Ok(Expression::Literal(Literal::Float(f)))
                } else {
                    let i: i64 = s.parse().map_err(|_| {
                        Error::parse(t.line, t.col, format!("invalid integer literal `{s}`"))
                    })?;
                    Ok(Expression::Literal(Literal::Integer(i)))
                }
            }
            Token::String(s) => {
                let s = s.clone();
                self.bump();
                Ok(Expression::Literal(Literal::String(s)))
            }
            Token::LParen => {
                self.bump();
                // Scalar subquery: `(SELECT ...)`
                if matches!(self.peek_tok(), Some(Token::KwSelect)) {
                    let inner = self.parse_select()?;
                    self.expect(Token::RParen)?;
                    return Ok(Expression::Scalar(Box::new(inner)));
                }
                let e = self.parse_expression()?;
                self.expect(Token::RParen)?;
                Ok(e)
            }
            Token::Ident(name) => {
                let name = name.clone();
                self.bump();
                // function call?
                if self.consume_if(&Token::LParen) {
                    let mut args = Vec::new();
                    let distinct = self.consume_if(&Token::KwDistinct);
                    if !matches!(self.peek_tok(), Some(Token::RParen)) {
                        if matches!(self.peek_tok(), Some(Token::Star)) {
                            // COUNT(*)
                            self.bump();
                            args.push(Expression::Wildcard);
                        } else {
                            loop {
                                args.push(self.parse_expression()?);
                                if !self.consume_if(&Token::Comma) {
                                    break;
                                }
                            }
                        }
                    }
                    self.expect(Token::RParen)?;
                    return Ok(Expression::Function {
                        name,
                        args,
                        distinct,
                    });
                }
                // qualified `t.col`?
                if self.consume_if(&Token::Dot) {
                    let col = self.expect_ident("column name")?;
                    return Ok(Expression::Qualified(name, col));
                }
                Ok(Expression::Column(name))
            }
            other => Err(Error::parse(
                t.line,
                t.col,
                format!("unexpected token in expression: {}", other.as_str()),
            )),
        }
    }

    fn parse_case(&mut self) -> Result<Expression> {
        self.expect(Token::KwCase)?;
        // Switch form: `CASE expr WHEN ... THEN ... ELSE ... END`.
        // Boolean form: `CASE WHEN cond THEN ... WHEN ... ELSE ... END`.
        let operand = if matches!(self.peek_tok(), Some(Token::KwWhen)) {
            None
        } else {
            Some(Box::new(self.parse_expression()?))
        };
        let mut branches = Vec::new();
        loop {
            self.expect(Token::KwWhen)?;
            let when = self.parse_expression()?;
            self.expect(Token::KwThen)?;
            let then = self.parse_expression()?;
            branches.push((when, then));
            if !matches!(self.peek_tok(), Some(Token::KwWhen)) {
                break;
            }
        }
        let otherwise = if self.consume_if(&Token::KwElse) {
            Some(Box::new(self.parse_expression()?))
        } else {
            None
        };
        self.expect(Token::KwEnd)?;
        Ok(Expression::Case {
            operand,
            branches,
            otherwise,
        })
    }

    // ------------------------------------------------------------------
    // Token helpers
    // ------------------------------------------------------------------

    fn peek(&self) -> Option<&Spanned> {
        self.tokens.get(self.pos)
    }

    fn peek_tok(&self) -> Option<&Token> {
        self.peek().map(|s| &s.token)
    }

    fn peek_tok_at(&self, off: usize) -> Option<&Token> {
        self.tokens.get(self.pos + off).map(|s| &s.token)
    }

    fn bump(&mut self) -> Option<Spanned> {
        let s = self.tokens.get(self.pos).cloned();
        if s.is_some() {
            self.pos += 1;
        }
        s
    }

    fn consume_if(&mut self, want: &Token) -> bool {
        if self.peek_tok() == Some(want) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, want: Token) -> Result<()> {
        match self.peek().cloned() {
            Some(s) if s.token == want => {
                self.pos += 1;
                Ok(())
            }
            Some(s) => Err(Error::parse(
                s.line,
                s.col,
                format!("expected {}, found {}", want.as_str(), s.token.as_str()),
            )),
            None => Err(self.err_here(format!("expected {}, found EOF", want.as_str()))),
        }
    }

    fn expect_ident(&mut self, what: &str) -> Result<String> {
        match self.peek().cloned() {
            Some(s) => match s.token {
                Token::Ident(name) => {
                    self.pos += 1;
                    Ok(name)
                }
                other => Err(Error::parse(
                    s.line,
                    s.col,
                    format!("expected {what}, found {}", other.as_str()),
                )),
            },
            None => Err(self.err_here(format!("expected {what}, found EOF"))),
        }
    }

    fn err_here(&self, msg: impl Into<String>) -> Error {
        if let Some(s) = self.peek() {
            Error::parse(s.line, s.col, msg)
        } else if let Some(last) = self.tokens.last() {
            Error::parse(last.line, last.col, msg)
        } else {
            Error::parse(0, 0, msg)
        }
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Statement {
        Parser::parse_one(s).unwrap_or_else(|e| panic!("parse error in `{s}`: {e}"))
    }

    fn parse_err(s: &str) -> String {
        Parser::parse_one(s).unwrap_err().to_string()
    }

    fn parse_expr(s: &str) -> Expression {
        let mut p = Parser::new(s).unwrap();
        let e = p
            .parse_expression()
            .unwrap_or_else(|e| panic!("expr error in `{s}`: {e}"));
        if let Some(rest) = p.peek().cloned() {
            panic!("trailing token after `{s}`: {}", rest.token.as_str());
        }
        e
    }

    fn lit_int(n: i64) -> Expression {
        Expression::Literal(Literal::Integer(n))
    }

    fn col(n: &str) -> Expression {
        Expression::Column(n.into())
    }

    fn bin(l: Expression, op: BinaryOp, r: Expression) -> Expression {
        Expression::Binary(Box::new(l), op, Box::new(r))
    }

    // -------- expressions ------------------------------------------------

    #[test]
    fn expr_integer() {
        assert_eq!(parse_expr("42"), lit_int(42));
    }

    #[test]
    fn expr_float() {
        assert_eq!(parse_expr("2.5"), Expression::Literal(Literal::Float(2.5)));
    }

    #[test]
    fn expr_arithmetic_left_assoc() {
        // 1 + 2 + 3 == ((1 + 2) + 3)
        let e = parse_expr("1 + 2 + 3");
        assert_eq!(
            e,
            bin(
                bin(lit_int(1), BinaryOp::Add, lit_int(2)),
                BinaryOp::Add,
                lit_int(3)
            )
        );
    }

    #[test]
    fn expr_precedence_mul_over_add() {
        // 1 + 2 * 3 == (1 + (2 * 3))
        let e = parse_expr("1 + 2 * 3");
        assert_eq!(
            e,
            bin(
                lit_int(1),
                BinaryOp::Add,
                bin(lit_int(2), BinaryOp::Mul, lit_int(3))
            )
        );
    }

    #[test]
    fn expr_exponent_right_assoc() {
        // 2 ^ 3 ^ 2 == (2 ^ (3 ^ 2))
        let e = parse_expr("2 ^ 3 ^ 2");
        assert_eq!(
            e,
            bin(
                lit_int(2),
                BinaryOp::Pow,
                bin(lit_int(3), BinaryOp::Pow, lit_int(2))
            )
        );
    }

    #[test]
    fn expr_unary_minus() {
        let e = parse_expr("-1");
        assert_eq!(e, Expression::Unary(UnaryOp::Minus, Box::new(lit_int(1))));
    }

    #[test]
    fn expr_unary_minus_with_mul() {
        // -1 * 2 -> ((-1) * 2)
        let e = parse_expr("-1 * 2");
        assert_eq!(
            e,
            bin(
                Expression::Unary(UnaryOp::Minus, Box::new(lit_int(1))),
                BinaryOp::Mul,
                lit_int(2)
            )
        );
    }

    #[test]
    fn expr_paren() {
        // (1 + 2) * 3
        let e = parse_expr("(1 + 2) * 3");
        assert_eq!(
            e,
            bin(
                bin(lit_int(1), BinaryOp::Add, lit_int(2)),
                BinaryOp::Mul,
                lit_int(3)
            )
        );
    }

    #[test]
    fn expr_bool_logic_precedence() {
        // a OR b AND c -> a OR (b AND c)
        let e = parse_expr("a OR b AND c");
        assert_eq!(
            e,
            bin(
                col("a"),
                BinaryOp::Or,
                bin(col("b"), BinaryOp::And, col("c"))
            )
        );
    }

    #[test]
    fn expr_not_binds_below_comparison() {
        // NOT a = 1 -> NOT (a = 1)
        let e = parse_expr("NOT a = 1");
        assert_eq!(
            e,
            Expression::Unary(
                UnaryOp::Not,
                Box::new(bin(col("a"), BinaryOp::Eq, lit_int(1)))
            )
        );
    }

    #[test]
    fn expr_string_concat() {
        let e = parse_expr("'a' || 'b' || 'c'");
        let s = |x: &str| Expression::Literal(Literal::String(x.into()));
        assert_eq!(
            e,
            bin(
                bin(s("a"), BinaryOp::Concat, s("b")),
                BinaryOp::Concat,
                s("c")
            )
        );
    }

    #[test]
    fn expr_comparison() {
        let e = parse_expr("a >= 5");
        assert_eq!(e, bin(col("a"), BinaryOp::GtEq, lit_int(5)));
    }

    #[test]
    fn expr_is_null_and_is_not_null() {
        assert_eq!(
            parse_expr("a IS NULL"),
            Expression::IsNull {
                expr: Box::new(col("a")),
                negated: false
            }
        );
        assert_eq!(
            parse_expr("a IS NOT NULL"),
            Expression::IsNull {
                expr: Box::new(col("a")),
                negated: true
            }
        );
    }

    #[test]
    fn expr_in_list() {
        let e = parse_expr("a IN (1, 2, 3)");
        match e {
            Expression::InList { list, negated, .. } => {
                assert_eq!(list.len(), 3);
                assert!(!negated);
            }
            _ => panic!("expected InList"),
        }
    }

    #[test]
    fn expr_not_in_list() {
        let e = parse_expr("a NOT IN (1, 2)");
        match e {
            Expression::InList { negated, .. } => assert!(negated),
            _ => panic!("expected InList"),
        }
    }

    #[test]
    fn expr_like() {
        let e = parse_expr("name LIKE 'a%'");
        match e {
            Expression::Like { negated, .. } => assert!(!negated),
            _ => panic!("expected Like"),
        }
    }

    #[test]
    fn expr_function_call() {
        let e = parse_expr("upper(name)");
        match e {
            Expression::Function {
                name,
                args,
                distinct,
            } => {
                assert_eq!(name, "upper");
                assert_eq!(args, vec![col("name")]);
                assert!(!distinct);
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn expr_count_star() {
        let e = parse_expr("count(*)");
        match e {
            Expression::Function {
                name,
                args,
                distinct,
            } => {
                assert_eq!(name, "count");
                assert_eq!(args, vec![Expression::Wildcard]);
                assert!(!distinct);
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn expr_count_distinct() {
        let e = parse_expr("count(DISTINCT id)");
        match e {
            Expression::Function { name, distinct, .. } => {
                assert_eq!(name, "count");
                assert!(distinct);
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn expr_qualified_column() {
        let e = parse_expr("t.id");
        assert_eq!(e, Expression::Qualified("t".into(), "id".into()));
    }

    // -------- statements -------------------------------------------------

    #[test]
    fn select_simple() {
        let s = parse("SELECT * FROM users");
        match s {
            Statement::Select(s) => {
                assert_eq!(s.items, vec![SelectItem::Wildcard]);
                assert!(matches!(
                    s.from,
                    Some(FromClause::Table { ref name, alias: None }) if name == "users"
                ));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn select_with_where_order_limit() {
        let s = parse("SELECT id, name FROM users WHERE age > 18 ORDER BY name DESC LIMIT 10");
        match s {
            Statement::Select(s) => {
                assert_eq!(s.items.len(), 2);
                assert!(s.r#where.is_some());
                assert_eq!(s.order_by.len(), 1);
                assert!(!s.order_by[0].asc);
                assert!(s.limit.is_some());
            }
            _ => panic!(),
        }
    }

    #[test]
    fn select_alias_and_qualified_wildcard() {
        let s = parse("SELECT u.*, u.id AS uid FROM users u");
        match s {
            Statement::Select(s) => {
                assert_eq!(s.items[0], SelectItem::QualifiedWildcard("u".into()));
                match &s.items[1] {
                    SelectItem::Expr { alias, .. } => assert_eq!(alias.as_deref(), Some("uid")),
                    _ => panic!(),
                }
                assert!(matches!(
                    s.from,
                    Some(FromClause::Table { alias: Some(ref a), .. }) if a == "u"
                ));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn select_inner_join() {
        let s = parse("SELECT u.id, o.total FROM users u INNER JOIN orders o ON u.id = o.user_id");
        match s {
            Statement::Select(s) => {
                let from = s.from.unwrap();
                assert!(matches!(
                    from,
                    FromClause::Join {
                        kind: JoinKind::Inner,
                        ..
                    }
                ));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn select_left_join() {
        let s = parse("SELECT * FROM a LEFT OUTER JOIN b ON a.id = b.id");
        match s {
            Statement::Select(s) => {
                let from = s.from.unwrap();
                assert!(matches!(
                    from,
                    FromClause::Join {
                        kind: JoinKind::Left,
                        ..
                    }
                ));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn select_group_by_having() {
        let s = parse("SELECT category, count(*) FROM items GROUP BY category HAVING count(*) > 1");
        match s {
            Statement::Select(s) => {
                assert_eq!(s.group_by.len(), 1);
                assert!(s.having.is_some());
            }
            _ => panic!(),
        }
    }

    #[test]
    fn insert_with_columns() {
        let s = parse("INSERT INTO t (a, b) VALUES (1, 'x'), (2, 'y')");
        match s {
            Statement::Insert(s) => {
                assert_eq!(s.table, "t");
                assert_eq!(s.columns, Some(vec!["a".into(), "b".into()]));
                match s.source {
                    InsertSource::Values(rows) => assert_eq!(rows.len(), 2),
                    _ => panic!("expected VALUES source"),
                }
            }
            _ => panic!(),
        }
    }

    #[test]
    fn insert_select_form() {
        let s = parse("INSERT INTO t (a, b) SELECT x, y FROM src");
        match s {
            Statement::Insert(s) => {
                assert!(matches!(s.source, InsertSource::Select(_)));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn insert_without_columns() {
        let s = parse("INSERT INTO t VALUES (1, 'x')");
        match s {
            Statement::Insert(s) => assert!(s.columns.is_none()),
            _ => panic!(),
        }
    }

    #[test]
    fn update_set_where() {
        let s = parse("UPDATE users SET name = 'bob', age = 30 WHERE id = 1");
        match s {
            Statement::Update(s) => {
                assert_eq!(s.assignments.len(), 2);
                assert!(s.r#where.is_some());
            }
            _ => panic!(),
        }
    }

    #[test]
    fn delete_with_where() {
        let s = parse("DELETE FROM users WHERE id = 1");
        match s {
            Statement::Delete(s) => assert!(s.r#where.is_some()),
            _ => panic!(),
        }
    }

    #[test]
    fn delete_all() {
        let s = parse("DELETE FROM users");
        match s {
            Statement::Delete(s) => assert!(s.r#where.is_none()),
            _ => panic!(),
        }
    }

    #[test]
    fn create_table_basic() {
        let s = parse(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, email TEXT UNIQUE)",
        );
        match s {
            Statement::CreateTable(c) => {
                assert_eq!(c.name, "users");
                assert!(!c.if_not_exists);
                assert_eq!(c.columns.len(), 3);
                assert!(c.columns[0].primary_key);
                assert!(!c.columns[0].nullable);
                assert!(!c.columns[1].nullable);
                assert!(c.columns[2].unique);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn create_table_if_not_exists_default() {
        let s = parse(
            "CREATE TABLE IF NOT EXISTS t (id INT, n FLOAT DEFAULT 0.5, ok BOOL DEFAULT TRUE)",
        );
        match s {
            Statement::CreateTable(c) => {
                assert!(c.if_not_exists);
                assert!(c.columns[1].default.is_some());
                assert!(c.columns[2].default.is_some());
            }
            _ => panic!(),
        }
    }

    #[test]
    fn create_table_varchar_length_ignored() {
        let s = parse("CREATE TABLE t (name VARCHAR(255))");
        match s {
            Statement::CreateTable(c) => {
                assert_eq!(c.columns[0].ty, DataType::String);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn drop_table() {
        let s = parse("DROP TABLE t");
        match s {
            Statement::DropTable(d) => {
                assert_eq!(d.name, "t");
                assert!(!d.if_exists);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn drop_table_if_exists() {
        let s = parse("DROP TABLE IF EXISTS t");
        match s {
            Statement::DropTable(d) => assert!(d.if_exists),
            _ => panic!(),
        }
    }

    #[test]
    fn create_index() {
        let s = parse("CREATE INDEX idx_users_age ON users(age)");
        match s {
            Statement::CreateIndex(i) => {
                assert_eq!(i.name, "idx_users_age");
                assert_eq!(i.table, "users");
                assert_eq!(i.column, "age");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn drop_index() {
        let s = parse("DROP INDEX idx_users_age");
        match s {
            Statement::DropIndex(i) => assert_eq!(i.name, "idx_users_age"),
            _ => panic!(),
        }
    }

    #[test]
    fn transaction_keywords() {
        assert_eq!(parse("BEGIN"), Statement::Begin);
        assert_eq!(parse("BEGIN TRANSACTION"), Statement::Begin);
        assert_eq!(parse("COMMIT"), Statement::Commit);
        assert_eq!(parse("ROLLBACK"), Statement::Rollback);
    }

    #[test]
    fn explain_wraps_inner() {
        let s = parse("EXPLAIN SELECT * FROM t");
        match s {
            Statement::Explain(inner) => match *inner {
                Statement::Select(_) => {}
                _ => panic!(),
            },
            _ => panic!(),
        }
    }

    #[test]
    fn parse_all_multi_statement() {
        let v = Parser::parse_all("SELECT 1; SELECT 2; SELECT 3").unwrap();
        assert_eq!(v.len(), 3);
    }

    #[test]
    fn parse_all_requires_statement_separator() {
        let e = Parser::parse_all("SELECT 1 SELECT 2").unwrap_err();
        assert!(e.to_string().contains("expected semicolon"), "{e}");
    }

    #[test]
    fn err_unexpected_token_carries_position() {
        let e = parse_err("SELECT FROM");
        assert!(e.contains("line 1, col 8"), "{e}");
    }

    #[test]
    fn err_missing_value() {
        let e = parse_err("INSERT INTO t VALUES");
        assert!(e.contains("expected"), "{e}");
    }

    #[test]
    fn err_trailing_input() {
        let e = parse_err("SELECT 1 +");
        assert!(e.contains("EOF"), "{e}");
    }

    #[test]
    fn expr_between() {
        let e = parse_expr("a BETWEEN 1 AND 10");
        match e {
            Expression::Between { negated, .. } => assert!(!negated),
            _ => panic!("expected Between"),
        }
    }

    #[test]
    fn expr_not_between() {
        let e = parse_expr("a NOT BETWEEN 1 AND 10");
        match e {
            Expression::Between { negated, .. } => assert!(negated),
            _ => panic!("expected Between"),
        }
    }
}
