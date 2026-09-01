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
    let tokens = super::lexer::lex(input).map_err(|e| ParseError { message: e.message, offset: e.offset })?;
    let mut p = Parser { tokens, pos: 0 };
    let mut stmts = Vec::new();
    while !p.at_eof() {
        stmts.push(p.parse_statement()?);
        p.expect_semi_or_eof()?;
    }
    Ok(stmts)
}

impl Parser {
    fn cur(&self) -> &Token { &self.tokens[self.pos].token }
    fn cur_offset(&self) -> usize { self.tokens[self.pos].offset }
    fn at_eof(&self) -> bool { matches!(self.cur(), Token::Eof) }

    fn advance(&mut self) -> Token {
        let t = self.tokens[self.pos].token.clone();
        if !matches!(t, Token::Eof) {
            self.pos += 1;
        }
        t
    }

    fn err<T>(&self, msg: impl Into<String>) -> Result<T, ParseError> {
        Err(ParseError { message: msg.into(), offset: self.cur_offset() })
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
            _ => self.err("expected a SQL statement"),
        }
    }

    fn parse_create(&mut self) -> Result<Statement, ParseError> {
        self.expect_keyword("CREATE")?;
        self.expect_keyword("TABLE")?;
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
            let ty = ColumnType::parse(&tyname)
                .ok_or_else(|| ParseError { message: format!("unknown type {tyname}"), offset: self.cur_offset() })?;
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
            columns.push(ColumnDef { name: cname, ty, primary_key, not_null, unique });
            if self.accept(&Token::Comma) {
                continue;
            }
            break;
        }
        self.expect(&Token::RParen)?;
        Ok(Statement::CreateTable { name, if_not_exists, columns })
    }

    fn parse_drop(&mut self) -> Result<Statement, ParseError> {
        self.expect_keyword("DROP")?;
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
        Ok(Statement::Insert { table, columns, rows })
    }

    fn parse_select(&mut self) -> Result<Statement, ParseError> {
        self.expect_keyword("SELECT")?;
        let distinct = self.accept_keyword("DISTINCT");
        let columns = if self.accept(&Token::Star) {
            SelectItems::Star
        } else {
            let mut items = Vec::new();
            loop {
                items.push(self.parse_expr(0)?);
                if self.accept(&Token::Comma) {
                    continue;
                }
                break;
            }
            SelectItems::List(items)
        };
        self.expect_keyword("FROM")?;
        let from = self.expect_ident()?;
        let where_clause = if self.accept_keyword("WHERE") {
            Some(self.parse_expr(0)?)
        } else {
            None
        };
        let mut order_by = Vec::new();
        if self.accept_keyword("ORDER") {
            self.expect_keyword("BY")?;
            loop {
                let col = self.expect_ident()?;
                let asc = if self.accept_keyword("DESC") {
                    false
                } else {
                    self.accept_keyword("ASC");
                    true
                };
                order_by.push((col, asc));
                if self.accept(&Token::Comma) {
                    continue;
                }
                break;
            }
        }
        let limit = if self.accept_keyword("LIMIT") {
            match self.advance() {
                Token::Integer(n) => Some(n as u64),
                _ => return self.err("LIMIT expects an integer"),
            }
        } else {
            None
        };
        Ok(Statement::Select { distinct, columns, from, where_clause, order_by, limit })
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
        Ok(Statement::Update { table, assignments, where_clause })
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
        Ok(Statement::Delete { table, where_clause })
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
            left = Expr::Binary { left: Box::new(left), op, right: Box::new(right) };
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        if self.accept_keyword("NOT") {
            let e = self.parse_expr(UNARY_PREC)?;
            return Ok(Expr::Unary { op: UnaryOp::Not, expr: Box::new(e) });
        }
        if self.accept(&Token::Minus) {
            let e = self.parse_expr(UNARY_PREC)?;
            return Ok(Expr::Unary { op: UnaryOp::Neg, expr: Box::new(e) });
        }
        let atom = self.parse_atom()?;
        if self.accept_keyword("IS") {
            let negated = self.accept_keyword("NOT");
            self.expect_keyword("NULL")?;
            return Ok(Expr::IsNull { expr: Box::new(atom), negated });
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
                Ok(Expr::Unary { op: UnaryOp::Neg, expr: Box::new(e) })
            }
            Token::Ident(s) => {
                self.advance();
                match s.to_uppercase().as_str() {
                    "NULL" => Ok(Expr::Literal(Value::Null)),
                    "TRUE" => Ok(Expr::Literal(Value::Boolean(true))),
                    "FALSE" => Ok(Expr::Literal(Value::Boolean(false))),
                    _ => Ok(Expr::Column(s)),
                }
            }
            Token::QuotedIdent(s) => {
                self.advance();
                Ok(Expr::Column(s))
            }
            _ => self.err("expected expression"),
        }
    }
}

const UNARY_PREC: u8 = 5;

fn bin_prec(op: BinOp) -> u8 {
    match op {
        BinOp::Or => 1,
        BinOp::And => 2,
        BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq => 3,
        BinOp::Add | BinOp::Sub => 4,
        BinOp::Mul | BinOp::Div | BinOp::Mod => 5,
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
        let s = one("CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score REAL, active BOOLEAN)");
        match s {
            Statement::CreateTable { name, if_not_exists, columns } => {
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
            Statement::Insert { table, columns, rows } => {
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
            Statement::Select { where_clause: Some(Expr::Binary { op: BinOp::Or, left, right }), .. } => {
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
            Statement::Select { where_clause: Some(Expr::Binary { op: BinOp::And, .. }), .. } => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_full_select_shape() {
        let s = one("SELECT DISTINCT name, score * 2 FROM users WHERE active IS NOT NULL ORDER BY score DESC, name LIMIT 10");
        match s {
            Statement::Select { distinct, columns, order_by, limit, .. } => {
                assert!(distinct);
                match columns {
                    SelectItems::List(items) => assert_eq!(items.len(), 2),
                    _ => panic!("expected list"),
                }
                assert_eq!(order_by, vec![("score".to_string(), false), ("name".to_string(), true)]);
                assert_eq!(limit, Some(10));
            }
            _ => panic!("wrong statement"),
        }
    }

    #[test]
    fn parses_update_delete_txn() {
        assert_eq!(one("UPDATE t SET a = 1, b = 'x' WHERE id = 3"), Statement::Update {
            table: "t".into(),
            assignments: vec![("a".into(), Expr::Literal(Value::Integer(1))), ("b".into(), Expr::Literal(Value::Text("x".into())))],
            where_clause: Some(Expr::Binary { left: Box::new(Expr::Column("id".into())), op: BinOp::Eq, right: Box::new(Expr::Literal(Value::Integer(3))) }),
        });
        assert!(matches!(one("DELETE FROM t"), Statement::Delete { where_clause: None, .. }));
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
        assert!(parse("SELECT * FROM 表").is_ok());
    }

    #[test]
    fn null_true_false_and_is_null() {
        let s = one("SELECT * FROM t WHERE a IS NULL AND b IS NOT NULL AND c = NULL");
        assert!(matches!(s, Statement::Select { .. }));
    }
}
