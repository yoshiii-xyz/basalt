/// Recursive-descent SQL parser with operator precedence.
use super::ast::*;
use super::lexer::{Token, TokenSpan};
use crate::types::{ColumnType, Value};

#[derive(Debug)]
pub struct ParseError {
    pub message: String,
    pub offset: usize,
}

pub struct Parser {
    tokens: Vec<TokenSpan>,
    pos: usize,
}

pub fn parse(input: &str) -> Result<Vec<Statement>, ParseError> {
    let tokens = super::lexer::lex(input).map_err(|e| ParseError {
        message: e.message,
        offset: e.offset,
    })?;
    let mut p = Parser { tokens, pos: 0 };
    let mut stmts = Vec::new();
    while !p.at_eof() {
        stmts.push(p.parse_statement()?);
        p.expect_semi_or_eof()?;
    }
    Ok(stmts)
}

impl Parser {
    fn cur(&self) -> &Token {
        &self.tokens[self.pos].token
    }
    fn cur_offset(&self) -> usize {
        self.tokens[self.pos].offset
    }
    fn at_eof(&self) -> bool {
        matches!(self.cur(), Token::Eof)
    }

    fn advance(&mut self) -> Token {
        let t = self.tokens[self.pos].token.clone();
        if !matches!(t, Token::Eof) {
            self.pos += 1;
        }
        t
    }

    fn err<T>(&self, msg: impl Into<String>) -> Result<T, ParseError> {
        Err(ParseError {
            message: msg.into(),
            offset: self.cur_offset(),
        })
    }

    fn accept_keyword(&mut self, kw: &str) -> bool {
        if let Token::Ident(s) = self.cur() {
            if s.eq_ignore_ascii_case(kw) {
                self.advance();
                return true;
            }
        }
        false
    }

    fn expect_keyword(&mut self, kw: &str) -> Result<(), ParseError> {
        if self.accept_keyword(kw) {
            Ok(())
        } else {
            self.err(format!("expected {kw}"))
        }
    }

    fn accept(&mut self, t: &Token) -> bool {
        if self.cur() == t {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, t: &Token) -> Result<(), ParseError> {
        if self.accept(t) {
            Ok(())
        } else {
            self.err(format!("expected {t:?}"))
        }
    }

    fn expect_ident(&mut self) -> Result<String, ParseError> {
        match self.cur().clone() {
            Token::Ident(s) => {
                self.advance();
                Ok(s)
            }
            Token::QuotedIdent(s) => {
                self.advance();
                Ok(s)
            }
            _ => self.err("expected identifier"),
        }
    }

    fn expect_semi_or_eof(&mut self) -> Result<(), ParseError> {
        if self.at_eof() {
            return Ok(());
        }
        self.expect(&Token::Semi)
    }

    fn parse_statement(&mut self) -> Result<Statement, ParseError> {
        match self.cur().clone() {
            Token::Ident(kw) if kw.eq_ignore_ascii_case("CREATE") => self.parse_create(),
            Token::Ident(kw) if kw.eq_ignore_ascii_case("DROP") => self.parse_drop(),
            Token::Ident(kw) if kw.eq_ignore_ascii_case("INSERT") => self.parse_insert(),
            Token::Ident(kw) if kw.eq_ignore_ascii_case("SELECT") => self.parse_select(),
            Token::Ident(kw) if kw.eq_ignore_ascii_case("UPDATE") => self.parse_update(),
            Token::Ident(kw) if kw.eq_ignore_ascii_case("DELETE") => self.parse_delete(),
            Token::Ident(kw) if kw.eq_ignore_ascii_case("BEGIN") => {
                self.advance();
                Ok(Statement::Begin)
            }
            Token::Ident(kw) if kw.eq_ignore_ascii_case("COMMIT") => {
                self.advance();
                Ok(Statement::Commit)
            }
            Token::Ident(kw) if kw.eq_ignore_ascii_case("ROLLBACK") => {
                self.advance();
                Ok(Statement::Rollback)
            }
            Token::Ident(kw) if kw.eq_ignore_ascii_case("CHECKPOINT") => {
                self.advance();
                Ok(Statement::Checkpoint)
            }
            Token::Ident(kw) if kw.eq_ignore_ascii_case("EXPLAIN") => {
                self.advance();
                Ok(Statement::Explain(Box::new(self.parse_statement()?)))
            }
            _ => self.err("expected a SQL statement"),
        }
    }

    fn parse_create(&mut self) -> Result<Statement, ParseError> {
        self.expect_keyword("CREATE")?;
        if self.accept_keyword("TABLE") {
            return self.parse_create_table_tail();
        }
        let unique = self.accept_keyword("UNIQUE");
        self.expect_keyword("INDEX")?;
        let if_not_exists = if self.accept_keyword("IF") {
            self.expect_keyword("NOT")?;
            self.expect_keyword("EXISTS")?;
            true
        } else {
            false
        };
        let name = self.expect_ident()?;
        self.expect_keyword("ON")?;
        let table = self.expect_ident()?;
        self.expect(&Token::LParen)?;
        let column = self.expect_ident()?;
        self.expect(&Token::RParen)?;
        Ok(Statement::CreateIndex {
            name,
            table,
            column,
            unique,
            if_not_exists,
        })
    }

    fn parse_create_table_tail(&mut self) -> Result<Statement, ParseError> {
        let if_not_exists = if self.accept_keyword("IF") {
            self.expect_keyword("NOT")?;
            self.expect_keyword("EXISTS")?;
            true
        } else {
            false
        };
        let name = self.expect_ident()?;
        self.expect(&Token::LParen)?;
        let mut columns = Vec::new();
        loop {
            let cname = self.expect_ident()?;
            let tyname = self.expect_ident()?;
            let ty = ColumnType::parse(&tyname).ok_or_else(|| ParseError {
                message: format!("unknown type {tyname}"),
                offset: self.cur_offset(),
            })?;
            let mut primary_key = false;
            let mut not_null = false;
            let mut unique = false;
            loop {
                if self.accept_keyword("PRIMARY") {
                    self.expect_keyword("KEY")?;
                    primary_key = true;
                    not_null = true;
                } else if self.accept_keyword("NOT") {
                    self.expect_keyword("NULL")?;
                    not_null = true;
                } else if self.accept_keyword("UNIQUE") {
                    unique = true;
                } else {
                    break;
                }
            }
            columns.push(ColumnDef {
                name: cname,
                ty,
                primary_key,
                not_null,
                unique,
            });
            if self.accept(&Token::Comma) {
                continue;
            }
            break;
        }
        self.expect(&Token::RParen)?;
        Ok(Statement::CreateTable {
            name,
            if_not_exists,
            columns,
        })
    }

    fn parse_drop(&mut self) -> Result<Statement, ParseError> {
        self.expect_keyword("DROP")?;
        if self.accept_keyword("INDEX") {
            let if_exists = if self.accept_keyword("IF") {
                self.expect_keyword("EXISTS")?;
                true
            } else {
                false
            };
            let name = self.expect_ident()?;
            return Ok(Statement::DropIndex { name, if_exists });
        }
        self.expect_keyword("TABLE")?;
        let if_exists = if self.accept_keyword("IF") {
            self.expect_keyword("EXISTS")?;
            true
        } else {
            false
        };
        let name = self.expect_ident()?;
        Ok(Statement::DropTable { name, if_exists })
    }

    fn parse_insert(&mut self) -> Result<Statement, ParseError> {
        self.expect_keyword("INSERT")?;
        self.expect_keyword("INTO")?;
        let table = self.expect_ident()?;
        let columns = if self.accept(&Token::LParen) {
            let mut cols = Vec::new();
            loop {
                cols.push(self.expect_ident()?);
                if self.accept(&Token::Comma) {
                    continue;
                }
                break;
            }
            self.expect(&Token::RParen)?;
            Some(cols)
        } else {
            None
        };
        if matches!(self.cur(), Token::Ident(keyword) if keyword.eq_ignore_ascii_case("SELECT")) {
            let query = self.parse_select()?;
            return Ok(Statement::InsertSelect {
                table,
                columns,
                query: Box::new(query),
            });
        }
        self.expect_keyword("VALUES")?;
        let mut rows = Vec::new();
        loop {
            self.expect(&Token::LParen)?;
            let mut row = Vec::new();
            loop {
                row.push(self.parse_expr(0)?);
                if self.accept(&Token::Comma) {
                    continue;
                }
                break;
            }
            self.expect(&Token::RParen)?;
            rows.push(row);
            if self.accept(&Token::Comma) {
                continue;
            }
            break;
        }
        Ok(Statement::Insert {
            table,
            columns,
            rows,
        })
    }

    fn parse_select(&mut self) -> Result<Statement, ParseError> {
        self.expect_keyword("SELECT")?;
        let distinct = self.accept_keyword("DISTINCT");
        let columns = if self.accept(&Token::Star) {
            SelectItems::Star
        } else {
            let mut items = Vec::new();
            loop {
                items.push(self.parse_select_item()?);
                if self.accept(&Token::Comma) {
                    continue;
                }
                break;
            }
            SelectItems::List(items)
        };
        let (from, from_alias) = if self.accept_keyword("FROM") {
            let from = self.expect_ident()?;
            let alias = self.parse_optional_alias()?;
            (from, alias)
        } else {
            (String::new(), None)
        };
        let mut joins = Vec::new();
        loop {
            let kind = if self.accept_keyword("JOIN") {
                Some(crate::sql::ast::JoinKind::Inner)
            } else if self.accept_keyword("INNER") {
                self.expect_keyword("JOIN")?;
                Some(crate::sql::ast::JoinKind::Inner)
            } else if self.accept_keyword("LEFT") {
                let _ = self.accept_keyword("OUTER");
                self.expect_keyword("JOIN")?;
                Some(crate::sql::ast::JoinKind::Left)
            } else if self.accept_keyword("RIGHT") {
                let _ = self.accept_keyword("OUTER");
                self.expect_keyword("JOIN")?;
                Some(crate::sql::ast::JoinKind::Right)
            } else if self.accept_keyword("FULL") {
                let _ = self.accept_keyword("OUTER");
                self.expect_keyword("JOIN")?;
                Some(crate::sql::ast::JoinKind::Full)
            } else if self.accept_keyword("CROSS") {
                self.expect_keyword("JOIN")?;
                Some(crate::sql::ast::JoinKind::Cross)
            } else {
                None
            };
            let Some(kind) = kind else { break };
            let table = self.expect_ident()?;
            let alias = self.parse_optional_alias()?;
            let on = if self.accept_keyword("ON") {
                if kind == crate::sql::ast::JoinKind::Cross {
                    return self.err("CROSS JOIN cannot have an ON condition");
                }
                Some(self.parse_expr(0)?)
            } else if kind == crate::sql::ast::JoinKind::Cross {
                None
            } else {
                return self.err("JOIN expects an ON condition");
            };
            joins.push(crate::sql::ast::JoinClause {
                kind,
                table,
                alias,
                on,
            });
        }
        let where_clause = if self.accept_keyword("WHERE") {
            Some(self.parse_expr(0)?)
        } else {
            None
        };
        let mut group_by = Vec::new();
        if self.accept_keyword("GROUP") {
            self.expect_keyword("BY")?;
            loop {
                group_by.push(self.parse_expr(0)?);
                if self.accept(&Token::Comma) {
                    continue;
                }
                break;
            }
        }
        let having = if self.accept_keyword("HAVING") {
            Some(self.parse_expr(0)?)
        } else {
            None
        };
        let mut order_by = Vec::new();
        let mut order_by_exprs = Vec::new();
        if self.accept_keyword("ORDER") {
            self.expect_keyword("BY")?;
            loop {
                let expression = self.parse_expr(0)?;
                let col = order_label(&expression);
                let asc = if self.accept_keyword("DESC") {
                    false
                } else {
                    self.accept_keyword("ASC");
                    true
                };
                order_by.push((col, asc));
                order_by_exprs.push((expression, asc));
                if self.accept(&Token::Comma) {
                    continue;
                }
                break;
            }
        }
        let limit = if self.accept_keyword("LIMIT") {
            match self.advance() {
                Token::Integer(n) if n >= 0 => Some(n as u64),
                _ => return self.err("LIMIT expects an integer"),
            }
        } else {
            None
        };
        let offset = if self.accept_keyword("OFFSET") {
            match self.advance() {
                Token::Integer(n) if n >= 0 => Some(n as u64),
                _ => return self.err("OFFSET expects a non-negative integer"),
            }
        } else {
            None
        };
        Ok(Statement::Select {
            distinct,
            columns,
            from,
            from_alias,
            joins,
            where_clause,
            group_by,
            having,
            order_by,
            order_by_exprs,
            limit,
            offset,
        })
    }

    fn parse_update(&mut self) -> Result<Statement, ParseError> {
        self.expect_keyword("UPDATE")?;
        let table = self.expect_ident()?;
        self.expect_keyword("SET")?;
        let mut assignments = Vec::new();
        loop {
            let col = self.expect_ident()?;
            self.expect(&Token::Eq)?;
            let val = self.parse_expr(0)?;
            assignments.push((col, val));
            if self.accept(&Token::Comma) {
                continue;
            }
            break;
        }
        let where_clause = if self.accept_keyword("WHERE") {
            Some(self.parse_expr(0)?)
        } else {
            None
        };
        Ok(Statement::Update {
            table,
            assignments,
            where_clause,
        })
    }

    fn parse_delete(&mut self) -> Result<Statement, ParseError> {
        self.expect_keyword("DELETE")?;
        self.expect_keyword("FROM")?;
        let table = self.expect_ident()?;
        let where_clause = if self.accept_keyword("WHERE") {
            Some(self.parse_expr(0)?)
        } else {
            None
        };
        Ok(Statement::Delete {
            table,
            where_clause,
        })
    }

    fn parse_select_item(&mut self) -> Result<Expr, ParseError> {
        let expr = self.parse_expr(0)?;
        let alias = if self.accept_keyword("AS") {
            Some(self.expect_ident()?)
        } else if let Token::Ident(name) = self.cur() {
            if !is_clause_keyword(name) {
                Some(self.expect_ident()?)
            } else {
                None
            }
        } else if matches!(self.cur(), Token::QuotedIdent(_)) {
            Some(self.expect_ident()?)
        } else {
            None
        };
        Ok(match alias {
            Some(alias) => Expr::Alias {
                expr: Box::new(expr),
                alias,
            },
            None => expr,
        })
    }

    fn parse_optional_alias(&mut self) -> Result<Option<String>, ParseError> {
        if self.accept_keyword("AS") {
            return Ok(Some(self.expect_ident()?));
        }
        match self.cur() {
            Token::Ident(name) if !is_clause_keyword(name) => Ok(Some(self.expect_ident()?)),
            Token::QuotedIdent(_) => Ok(Some(self.expect_ident()?)),
            _ => Ok(None),
        }
    }

    // ---- expressions (precedence climbing) ----

    pub fn parse_expr(&mut self, min_prec: u8) -> Result<Expr, ParseError> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.cur() {
                Token::Eq => BinOp::Eq,
                Token::NotEq => BinOp::NotEq,
                Token::Lt => BinOp::Lt,
                Token::LtEq => BinOp::LtEq,
                Token::Gt => BinOp::Gt,
                Token::GtEq => BinOp::GtEq,
                Token::Plus => BinOp::Add,
                Token::Minus => BinOp::Sub,
                Token::Star => BinOp::Mul,
                Token::Slash => BinOp::Div,
                Token::Percent => BinOp::Mod,
                Token::Ident(s) if s.eq_ignore_ascii_case("AND") => BinOp::And,
                Token::Ident(s) if s.eq_ignore_ascii_case("OR") => BinOp::Or,
                _ => break,
            };
            let prec = bin_prec(op);
            if prec < min_prec {
                break;
            }
            self.advance();
            // comparison ops are non-associative: parse right side at prec+1
            let right = self.parse_expr(prec + 1)?;
            left = Expr::Binary {
                left: Box::new(left),
                op,
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        if self.accept_keyword("NOT") {
            let e = self.parse_expr(NOT_PREC)?;
            return Ok(Expr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(e),
            });
        }
        if self.accept(&Token::Minus) {
            let e = self.parse_expr(UNARY_PREC)?;
            return Ok(Expr::Unary {
                op: UnaryOp::Neg,
                expr: Box::new(e),
            });
        }
        let atom = self.parse_atom()?;
        if self.accept_keyword("IS") {
            let negated = self.accept_keyword("NOT");
            self.expect_keyword("NULL")?;
            return Ok(Expr::IsNull {
                expr: Box::new(atom),
                negated,
            });
        }
        Ok(atom)
    }

    fn parse_atom(&mut self) -> Result<Expr, ParseError> {
        match self.cur().clone() {
            Token::Integer(n) => {
                self.advance();
                Ok(Expr::Literal(Value::Integer(n)))
            }
            Token::Real(f) => {
                self.advance();
                Ok(Expr::Literal(Value::Real(f)))
            }
            Token::Str(s) => {
                self.advance();
                Ok(Expr::Literal(Value::Text(s)))
            }
            Token::LParen => {
                self.advance();
                let e = self.parse_expr(0)?;
                self.expect(&Token::RParen)?;
                Ok(e)
            }
            Token::Minus => {
                self.advance();
                let e = self.parse_expr(UNARY_PREC)?;
                Ok(Expr::Unary {
                    op: UnaryOp::Neg,
                    expr: Box::new(e),
                })
            }
            Token::Ident(s) => {
                self.advance();
                match s.to_uppercase().as_str() {
                    "NULL" => Ok(Expr::Literal(Value::Null)),
                    "TRUE" => Ok(Expr::Literal(Value::Boolean(true))),
                    "FALSE" => Ok(Expr::Literal(Value::Boolean(false))),
                    _ if self.accept(&Token::LParen) => {
                        let distinct = self.accept_keyword("DISTINCT");
                        let mut args = Vec::new();
                        if !self.accept(&Token::RParen) {
                            if self.accept(&Token::Star) {
                                args.push(Expr::Column("*".into()));
                            } else {
                                loop {
                                    args.push(self.parse_expr(0)?);
                                    if self.accept(&Token::Comma) {
                                        continue;
                                    }
                                    break;
                                }
                            }
                            self.expect(&Token::RParen)?;
                        }
                        Ok(Expr::Function {
                            name: s,
                            args,
                            distinct,
                        })
                    }
                    _ if self.accept(&Token::Dot) => {
                        if self.accept(&Token::Star) {
                            Ok(Expr::QualifiedWildcard(s))
                        } else {
                            let column = self.expect_ident()?;
                            Ok(Expr::ColumnRef {
                                relation: s,
                                column,
                            })
                        }
                    }
                    _ => Ok(Expr::Column(s)),
                }
            }
            Token::QuotedIdent(s) => {
                self.advance();
                if self.accept(&Token::Dot) {
                    if self.accept(&Token::Star) {
                        Ok(Expr::QualifiedWildcard(s))
                    } else {
                        let column = self.expect_ident()?;
                        Ok(Expr::ColumnRef {
                            relation: s,
                            column,
                        })
                    }
                } else {
                    Ok(Expr::Column(s))
                }
            }
            _ => self.err("expected expression"),
        }
    }
}

const NOT_PREC: u8 = 3;
const UNARY_PREC: u8 = 6;

fn bin_prec(op: BinOp) -> u8 {
    match op {
        BinOp::Or => 1,
        BinOp::And => 2,
        BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq => 3,
        BinOp::Add | BinOp::Sub => 4,
        BinOp::Mul | BinOp::Div | BinOp::Mod => 5,
    }
}

fn is_clause_keyword(value: &str) -> bool {
    matches!(
        value.to_ascii_uppercase().as_str(),
        "FROM"
            | "WHERE"
            | "GROUP"
            | "HAVING"
            | "ORDER"
            | "LIMIT"
            | "OFFSET"
            | "JOIN"
            | "INNER"
            | "LEFT"
            | "RIGHT"
            | "FULL"
            | "OUTER"
            | "CROSS"
            | "ON"
            | "ASC"
            | "DESC"
            | "AND"
            | "OR"
    )
}

fn order_label(expr: &Expr) -> String {
    match expr {
        Expr::Column(name) => name.clone(),
        Expr::ColumnRef { relation, column } => format!("{relation}.{column}"),
        Expr::Function { name, .. } => name.clone(),
        _ => "expr".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(input: &str) -> Statement {
        let mut stmts = parse(input).expect("parse failed");
        assert_eq!(stmts.len(), 1);
        stmts.pop().unwrap()
    }

    #[test]
    fn parses_create_table() {
        let s = one(
            "CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score REAL, active BOOLEAN)",
        );
        match s {
            Statement::CreateTable {
                name,
                if_not_exists,
                columns,
            } => {
                assert_eq!(name, "users");
                assert!(if_not_exists);
                assert_eq!(columns.len(), 4);
                assert!(columns[0].primary_key);
                assert!(columns[1].not_null);
                assert_eq!(columns[2].ty, ColumnType::Real);
            }
            _ => panic!("wrong statement"),
        }
    }

    #[test]
    fn parses_insert_multi_row() {
        let s = one("INSERT INTO t (a, b) VALUES (1, 'x'), (2, 'y''s')");
        match s {
            Statement::Insert {
                table,
                columns,
                rows,
            } => {
                assert_eq!(table, "t");
                assert_eq!(columns.unwrap(), vec!["a".to_string(), "b".to_string()]);
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[1][1], Expr::Literal(Value::Text("y's".into())));
            }
            _ => panic!("wrong statement"),
        }
    }

    #[test]
    fn operator_precedence_is_correct() {
        // 1 + 2 * 3 == 1 + (2*3), and a AND b OR c == (a AND b) OR c
        let s = one("SELECT * FROM t WHERE a = 1 + 2 * 3 AND b OR c");
        match s {
            Statement::Select {
                where_clause:
                    Some(Expr::Binary {
                        op: BinOp::Or,
                        left,
                        right,
                    }),
                ..
            } => {
                assert!(matches!(*left, Expr::Binary { op: BinOp::And, .. }));
                assert_eq!(*right, Expr::Column("c".into()));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn comparison_binds_tighter_than_and() {
        let s = one("SELECT * FROM t WHERE x < 10 AND y > 2");
        match s {
            Statement::Select {
                where_clause: Some(Expr::Binary { op: BinOp::And, .. }),
                ..
            } => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_full_select_shape() {
        let s = one(
            "SELECT DISTINCT name, score * 2 FROM users WHERE active IS NOT NULL ORDER BY score DESC, name LIMIT 10",
        );
        match s {
            Statement::Select {
                distinct,
                columns,
                order_by,
                limit,
                ..
            } => {
                assert!(distinct);
                match columns {
                    SelectItems::List(items) => assert_eq!(items.len(), 2),
                    _ => panic!("expected list"),
                }
                assert_eq!(
                    order_by,
                    vec![("score".to_string(), false), ("name".to_string(), true)]
                );
                assert_eq!(limit, Some(10));
            }
            _ => panic!("wrong statement"),
        }
    }

    #[test]
    fn parses_update_delete_txn() {
        assert_eq!(
            one("UPDATE t SET a = 1, b = 'x' WHERE id = 3"),
            Statement::Update {
                table: "t".into(),
                assignments: vec![
                    ("a".into(), Expr::Literal(Value::Integer(1))),
                    ("b".into(), Expr::Literal(Value::Text("x".into())))
                ],
                where_clause: Some(Expr::Binary {
                    left: Box::new(Expr::Column("id".into())),
                    op: BinOp::Eq,
                    right: Box::new(Expr::Literal(Value::Integer(3)))
                }),
            }
        );
        assert!(matches!(
            one("DELETE FROM t"),
            Statement::Delete {
                where_clause: None,
                ..
            }
        ));
        assert_eq!(one("BEGIN"), Statement::Begin);
        assert_eq!(one("ROLLBACK"), Statement::Rollback);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("SELEC * FROM t").is_err());
        assert!(parse("INSERT INTO t VALUES (").is_err());
        assert!(parse("SELECT * FROM t WHERE x = 'unterminated").is_err());
        assert!(parse("CREATE TABLE t (a BOGUS)").is_err());
        assert!(parse("SELECT * FROM t WHERE 1 = 1 == 2").is_err()); // dangling ==
    }

    #[test]
    fn multiple_statements_and_comments() {
        let stmts = parse("-- hello\nBEGIN; /* block */ SELECT * FROM t; COMMIT;").unwrap();
        assert_eq!(stmts.len(), 3);
    }

    #[test]
    fn quoted_and_unicode_identifiers() {
        let s = one("SELECT * FROM \"my table\"");
        match s {
            Statement::Select { from, .. } => assert_eq!(from, "my table"),
            _ => panic!("wrong statement"),
        }
        let s = one("SELECT \"u\".\"display name\" \"friendly name\" FROM \"my table\" \"u\"");
        match s {
            Statement::Select {
                columns: SelectItems::List(items),
                from,
                from_alias,
                ..
            } => {
                assert_eq!(from, "my table");
                assert_eq!(from_alias, Some("u".into()));
                assert!(matches!(
                    &items[0],
                    Expr::Alias { alias, expr }
                        if alias == "friendly name"
                            && matches!(expr.as_ref(), Expr::ColumnRef { relation, column } if relation == "u" && column == "display name")
                ));
            }
            _ => panic!("wrong statement shape"),
        }
        assert!(parse("SELECT * FROM 表").is_ok());
    }

    #[test]
    fn null_true_false_and_is_null() {
        let s = one("SELECT * FROM t WHERE a IS NULL AND b IS NOT NULL AND c = NULL");
        assert!(matches!(s, Statement::Select { .. }));
    }

    #[test]
    fn parses_indexes_joins_groups_and_functions() {
        assert!(matches!(
            one("CREATE UNIQUE INDEX IF NOT EXISTS ix ON users (email)"),
            Statement::CreateIndex {
                unique: true,
                if_not_exists: true,
                ..
            }
        ));
        assert!(matches!(
            one("DROP INDEX IF EXISTS ix"),
            Statement::DropIndex {
                if_exists: true,
                ..
            }
        ));
        let statement = one(
            "SELECT u.name AS user_name, COUNT(DISTINCT p.id) AS posts FROM users u LEFT JOIN posts p ON u.id = p.user_id GROUP BY u.name HAVING COUNT(*) > 0 ORDER BY u.name LIMIT 5 OFFSET 2",
        );
        let Statement::Select {
            from_alias,
            joins,
            group_by,
            having,
            limit,
            offset,
            ..
        } = statement
        else {
            panic!()
        };
        assert_eq!(from_alias, Some("u".into()));
        assert_eq!(joins.len(), 1);
        assert_eq!(group_by.len(), 1);
        assert!(having.is_some());
        assert_eq!(limit, Some(5));
        assert_eq!(offset, Some(2));
    }

    #[test]
    fn not_binds_around_comparison() {
        let statement = one("SELECT * FROM t WHERE NOT id = 1 AND id = 2");
        let Statement::Select {
            where_clause:
                Some(Expr::Binary {
                    left,
                    op: BinOp::And,
                    ..
                }),
            ..
        } = statement
        else {
            panic!()
        };
        assert!(matches!(
            *left,
            Expr::Unary {
                op: UnaryOp::Not,
                expr
            } if matches!(*expr, Expr::Binary { op: BinOp::Eq, .. })
        ));
    }
}
