use crate::types::{ColumnType, Value};

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    CreateTable {
        name: String,
        if_not_exists: bool,
        columns: Vec<ColumnDef>,
    },
    DropTable {
        name: String,
        if_exists: bool,
    },
    CreateIndex {
        name: String,
        table: String,
        column: String,
        unique: bool,
        if_not_exists: bool,
    },
    DropIndex {
        name: String,
        if_exists: bool,
    },
    Insert {
        table: String,
        columns: Option<Vec<String>>,
        rows: Vec<Vec<Expr>>,
    },
    InsertSelect {
        table: String,
        columns: Option<Vec<String>>,
        query: Box<Statement>,
    },
    Select {
        distinct: bool,
        columns: SelectItems,
        from: String,
        from_alias: Option<String>,
        joins: Vec<JoinClause>,
        where_clause: Option<Expr>,
        group_by: Vec<Expr>,
        having: Option<Expr>,
        order_by: Vec<(String, bool)>, // col, ascending
        order_by_exprs: Vec<(Expr, bool)>,
        limit: Option<u64>,
        offset: Option<u64>,
    },
    Update {
        table: String,
        assignments: Vec<(String, Expr)>,
        where_clause: Option<Expr>,
    },
    Delete {
        table: String,
        where_clause: Option<Expr>,
    },
    Begin,
    Commit,
    Rollback,
    Checkpoint,
    Explain(Box<Statement>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct JoinClause {
    pub kind: JoinKind,
    pub table: String,
    pub alias: Option<String>,
    pub on: Option<Expr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectItems {
    Star,
    List(Vec<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub ty: ColumnType,
    pub primary_key: bool,
    pub not_null: bool,
    pub unique: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Value),
    Column(String),
    QualifiedWildcard(String),
    ColumnRef {
        relation: String,
        column: String,
    },
    Alias {
        expr: Box<Expr>,
        alias: String,
    },
    Function {
        name: String,
        args: Vec<Expr>,
        distinct: bool,
    },
    Binary {
        left: Box<Expr>,
        op: BinOp,
        right: Box<Expr>,
    },
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Not,
    Neg,
}
