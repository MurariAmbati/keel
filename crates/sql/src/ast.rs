use keel_types::{ColumnType, Value};

#[derive(Clone, Debug, PartialEq)]
pub enum Stmt {
    CreateTable(CreateTable),
    CreateIndex(CreateIndex),
    Insert(Insert),
    Delete(Delete),
    Update(Update),
    Select(Select),
    DropTable(String),
    DropIndex(String),
    Begin,
    Commit,
    Rollback,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub ty: ColumnType,
    pub not_null: bool,
    pub primary_key: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CreateTable {
    pub name: String,
    pub columns: Vec<ColumnDef>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CreateIndex {
    pub name: String,
    pub table: String,
    pub columns: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Insert {
    pub table: String,
    pub columns: Option<Vec<String>>,
    pub rows: Vec<Vec<Expr>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Delete {
    pub table: String,
    pub filter: Option<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Update {
    pub table: String,
    pub assignments: Vec<(String, Expr)>,
    pub filter: Option<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Select {
    pub distinct: bool,
    pub items: Vec<SelectItem>,
    pub from: Option<FromClause>,
    pub filter: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    pub order_by: Vec<OrderKey>,
    pub limit: Option<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SelectItem {
    Wildcard,
    QualifiedWildcard(String),
    Expr(Expr, Option<String>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct OrderKey {
    pub expr: Expr,
    pub asc: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TableRef {
    pub table: String,
    pub alias: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FromClause {
    pub first: TableRef,
    pub joins: Vec<(JoinKind, TableRef, Expr)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Not,
    Neg,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Column {
        table: Option<String>,
        name: String,
    },
    Literal(Value),
    Binary {
        op: BinOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Unary {
        op: UnOp,
        expr: Box<Expr>,
    },
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    InSubquery {
        expr: Box<Expr>,
        query: Box<Select>,
        negated: bool,
    },
    ScalarSubquery(Box<Select>),
    Case {
        operand: Option<Box<Expr>>,
        whens: Vec<(Expr, Expr)>,
        els: Option<Box<Expr>>,
    },
    Aggregate {
        func: AggFunc,
        arg: Option<Box<Expr>>,
        distinct: bool,
    },
}

impl Expr {
    pub fn col(name: &str) -> Expr {
        Expr::Column {
            table: None,
            name: name.to_string(),
        }
    }
    pub fn qcol(table: &str, name: &str) -> Expr {
        Expr::Column {
            table: Some(table.to_string()),
            name: name.to_string(),
        }
    }
    pub fn bin(op: BinOp, l: Expr, r: Expr) -> Expr {
        Expr::Binary {
            op,
            left: Box::new(l),
            right: Box::new(r),
        }
    }
}
