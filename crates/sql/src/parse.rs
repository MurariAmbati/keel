//! Recursive-descent parser over the freeze grammar (§6.1, D10).
//!
//! Precedence (lowest→highest): `OR`, `AND`, `NOT`, comparisons (`= <> < <= > >=`,
//! `IS [NOT] NULL`, `[NOT] IN`), additive (`+ -`), multiplicative (`* /`), unary
//! `-`, then atoms (literals, columns, `(...)`, `CASE`, aggregates, subqueries).

use keel_types::{ColumnType, Value};

use crate::ast::*;
use crate::lex::{lex, Tok, Token};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub pos: usize,
    pub msg: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "parse error at {}: {}", self.pos, self.msg)
    }
}
impl std::error::Error for ParseError {}

impl From<crate::lex::LexError> for ParseError {
    fn from(e: crate::lex::LexError) -> Self {
        ParseError {
            pos: e.pos,
            msg: e.msg,
        }
    }
}

type PResult<T> = Result<T, ParseError>;

struct Parser {
    toks: Vec<Token>,
    i: usize,
}

/// Parse a single statement (an optional trailing `;` is allowed).
pub fn parse_statement(src: &str) -> PResult<Stmt> {
    let mut p = Parser {
        toks: lex(src)?,
        i: 0,
    };
    let s = p.statement()?;
    if p.peek() == &Tok::Semicolon {
        p.bump();
    }
    p.expect_eof()?;
    Ok(s)
}

/// Parse a bare expression (handy for tests and the query generator).
pub fn parse_expr(src: &str) -> PResult<Expr> {
    let mut p = Parser {
        toks: lex(src)?,
        i: 0,
    };
    let e = p.expr()?;
    p.expect_eof()?;
    Ok(e)
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.i].tok
    }
    fn peek_pos(&self) -> usize {
        self.toks[self.i].pos
    }
    fn bump(&mut self) -> Tok {
        let t = self.toks[self.i].tok.clone();
        if self.i + 1 < self.toks.len() {
            self.i += 1;
        }
        t
    }
    fn err<T>(&self, msg: impl Into<String>) -> PResult<T> {
        Err(ParseError {
            pos: self.peek_pos(),
            msg: msg.into(),
        })
    }
    fn expect_eof(&self) -> PResult<()> {
        if self.peek() == &Tok::Eof {
            Ok(())
        } else {
            Err(ParseError {
                pos: self.peek_pos(),
                msg: format!("unexpected trailing input {:?}", self.peek()),
            })
        }
    }
    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == t {
            self.bump();
            true
        } else {
            false
        }
    }
    fn expect(&mut self, t: &Tok) -> PResult<()> {
        if self.eat(t) {
            Ok(())
        } else {
            self.err(format!("expected {t:?}, found {:?}", self.peek()))
        }
    }
    /// True and consumes if the next token is the keyword `kw`.
    fn eat_kw(&mut self, kw: &str) -> bool {
        if let Tok::Word(w) = self.peek() {
            if w == kw {
                self.bump();
                return true;
            }
        }
        false
    }
    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Tok::Word(w) if w == kw)
    }
    fn expect_kw(&mut self, kw: &str) -> PResult<()> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            self.err(format!("expected keyword '{kw}', found {:?}", self.peek()))
        }
    }
    fn ident(&mut self) -> PResult<String> {
        match self.peek().clone() {
            Tok::Word(w) if !is_reserved(&w) => {
                self.bump();
                Ok(w)
            }
            other => self.err(format!("expected identifier, found {other:?}")),
        }
    }

    fn statement(&mut self) -> PResult<Stmt> {
        if self.eat_kw("create") {
            if self.eat_kw("table") {
                return Ok(Stmt::CreateTable(self.create_table()?));
            }
            if self.eat_kw("index") {
                return Ok(Stmt::CreateIndex(self.create_index()?));
            }
            return self.err("expected TABLE or INDEX after CREATE");
        }
        if self.eat_kw("insert") {
            return Ok(Stmt::Insert(self.insert()?));
        }
        if self.eat_kw("drop") {
            if self.eat_kw("table") {
                return Ok(Stmt::DropTable(self.ident()?));
            }
            if self.eat_kw("index") {
                return Ok(Stmt::DropIndex(self.ident()?));
            }
            return self.err("expected TABLE or INDEX after DROP");
        }
        if self.eat_kw("delete") {
            return Ok(Stmt::Delete(self.delete()?));
        }
        if self.eat_kw("update") {
            return Ok(Stmt::Update(self.update()?));
        }
        if self.is_kw("select") {
            return Ok(Stmt::Select(self.select()?));
        }
        if self.eat_kw("begin") {
            return Ok(Stmt::Begin);
        }
        if self.eat_kw("commit") {
            return Ok(Stmt::Commit);
        }
        if self.eat_kw("rollback") {
            return Ok(Stmt::Rollback);
        }
        self.err(format!("unexpected {:?} at statement start", self.peek()))
    }

    fn create_table(&mut self) -> PResult<CreateTable> {
        let name = self.ident()?;
        self.expect(&Tok::LParen)?;
        let mut columns = Vec::new();
        loop {
            let cname = self.ident()?;
            let ty = self.column_type()?;
            let mut not_null = false;
            let mut primary_key = false;
            loop {
                if self.eat_kw("not") {
                    self.expect_kw("null")?;
                    not_null = true;
                } else if self.eat_kw("primary") {
                    self.expect_kw("key")?;
                    primary_key = true;
                    not_null = true;
                } else {
                    break;
                }
            }
            columns.push(ColumnDef {
                name: cname,
                ty,
                not_null,
                primary_key,
            });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(&Tok::RParen)?;
        Ok(CreateTable { name, columns })
    }

    fn column_type(&mut self) -> PResult<ColumnType> {
        let w = match self.peek().clone() {
            Tok::Word(w) => {
                self.bump();
                w
            }
            other => return self.err(format!("expected a type, found {other:?}")),
        };
        Ok(match w.as_str() {
            "int" | "integer" => ColumnType::Int,
            "bigint" => ColumnType::BigInt,
            "double" | "float" | "real" => ColumnType::Double,
            "bool" | "boolean" => ColumnType::Bool,
            "varchar" | "text" | "char" => {
                let n = if self.eat(&Tok::LParen) {
                    let n = self.int_lit()?;
                    self.expect(&Tok::RParen)?;
                    n.clamp(0, u16::MAX as i64) as u16
                } else {
                    65535
                };
                ColumnType::Varchar(n)
            }
            other => return self.err(format!("unknown type '{other}'")),
        })
    }

    fn int_lit(&mut self) -> PResult<i64> {
        match self.peek().clone() {
            Tok::Int(n) => {
                self.bump();
                Ok(n)
            }
            other => self.err(format!("expected an integer, found {other:?}")),
        }
    }

    fn create_index(&mut self) -> PResult<CreateIndex> {
        let name = self.ident()?;
        self.expect_kw("on")?;
        let table = self.ident()?;
        self.expect(&Tok::LParen)?;
        let mut columns = vec![self.ident()?];
        while self.eat(&Tok::Comma) {
            columns.push(self.ident()?);
        }
        self.expect(&Tok::RParen)?;
        Ok(CreateIndex {
            name,
            table,
            columns,
        })
    }

    fn insert(&mut self) -> PResult<Insert> {
        self.expect_kw("into")?;
        let table = self.ident()?;
        let columns = if self.eat(&Tok::LParen) {
            let mut cols = vec![self.ident()?];
            while self.eat(&Tok::Comma) {
                cols.push(self.ident()?);
            }
            self.expect(&Tok::RParen)?;
            Some(cols)
        } else {
            None
        };
        self.expect_kw("values")?;
        let mut rows = Vec::new();
        loop {
            self.expect(&Tok::LParen)?;
            let mut row = vec![self.expr()?];
            while self.eat(&Tok::Comma) {
                row.push(self.expr()?);
            }
            self.expect(&Tok::RParen)?;
            rows.push(row);
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        Ok(Insert {
            table,
            columns,
            rows,
        })
    }

    fn delete(&mut self) -> PResult<Delete> {
        self.expect_kw("from")?;
        let table = self.ident()?;
        let filter = if self.eat_kw("where") {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(Delete { table, filter })
    }

    fn update(&mut self) -> PResult<Update> {
        let table = self.ident()?;
        self.expect_kw("set")?;
        let mut assignments = Vec::new();
        loop {
            let col = self.ident()?;
            self.expect(&Tok::Eq)?;
            let val = self.expr()?;
            assignments.push((col, val));
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        let filter = if self.eat_kw("where") {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(Update {
            table,
            assignments,
            filter,
        })
    }

    fn select(&mut self) -> PResult<Select> {
        self.expect_kw("select")?;
        let distinct = self.eat_kw("distinct");
        let mut items = Vec::new();
        loop {
            items.push(self.select_item()?);
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        let from = if self.eat_kw("from") {
            Some(self.from_clause()?)
        } else {
            None
        };
        let filter = if self.eat_kw("where") {
            Some(self.expr()?)
        } else {
            None
        };
        let mut group_by = Vec::new();
        if self.eat_kw("group") {
            self.expect_kw("by")?;
            group_by.push(self.expr()?);
            while self.eat(&Tok::Comma) {
                group_by.push(self.expr()?);
            }
        }
        let having = if self.eat_kw("having") {
            Some(self.expr()?)
        } else {
            None
        };
        let mut order_by = Vec::new();
        if self.eat_kw("order") {
            self.expect_kw("by")?;
            loop {
                let expr = self.expr()?;
                let asc = if self.eat_kw("desc") {
                    false
                } else {
                    self.eat_kw("asc");
                    true
                };
                order_by.push(OrderKey { expr, asc });
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        let limit = if self.eat_kw("limit") {
            Some(self.int_lit()?)
        } else {
            None
        };
        Ok(Select {
            distinct,
            items,
            from,
            filter,
            group_by,
            having,
            order_by,
            limit,
        })
    }

    fn select_item(&mut self) -> PResult<SelectItem> {
        if self.eat(&Tok::Star) {
            return Ok(SelectItem::Wildcard);
        }
        if let Tok::Word(w) = self.peek().clone() {
            if !is_reserved(&w)
                && self.toks.get(self.i + 1).map(|t| &t.tok) == Some(&Tok::Dot)
                && self.toks.get(self.i + 2).map(|t| &t.tok) == Some(&Tok::Star)
            {
                self.bump();
                self.bump();
                self.bump();
                return Ok(SelectItem::QualifiedWildcard(w));
            }
        }
        let e = self.expr()?;
        let alias = if self.eat_kw("as") {
            Some(self.ident()?)
        } else if let Tok::Word(w) = self.peek().clone() {
            if !is_reserved(&w) {
                self.bump();
                Some(w)
            } else {
                None
            }
        } else {
            None
        };
        Ok(SelectItem::Expr(e, alias))
    }

    #[allow(clippy::wrong_self_convention)]
    fn from_clause(&mut self) -> PResult<FromClause> {
        let first = self.table_ref()?;
        let mut joins = Vec::new();
        loop {
            let kind = if self.eat_kw("left") {
                self.eat_kw("outer");
                self.expect_kw("join")?;
                JoinKind::Left
            } else if self.eat_kw("inner") {
                self.expect_kw("join")?;
                JoinKind::Inner
            } else if self.eat_kw("join") {
                JoinKind::Inner
            } else if self.eat(&Tok::Comma) {
                let t = self.table_ref()?;
                joins.push((JoinKind::Inner, t, Expr::Literal(Value::Bool(true))));
                continue;
            } else {
                break;
            };
            let t = self.table_ref()?;
            self.expect_kw("on")?;
            let on = self.expr()?;
            joins.push((kind, t, on));
        }
        Ok(FromClause { first, joins })
    }

    fn table_ref(&mut self) -> PResult<TableRef> {
        let table = self.ident()?;
        let alias = if self.eat_kw("as") {
            Some(self.ident()?)
        } else if let Tok::Word(w) = self.peek().clone() {
            if !is_reserved(&w) {
                self.bump();
                Some(w)
            } else {
                None
            }
        } else {
            None
        };
        Ok(TableRef { table, alias })
    }

    pub fn expr(&mut self) -> PResult<Expr> {
        self.or_expr()
    }

    fn or_expr(&mut self) -> PResult<Expr> {
        let mut e = self.and_expr()?;
        while self.eat_kw("or") {
            let r = self.and_expr()?;
            e = Expr::bin(BinOp::Or, e, r);
        }
        Ok(e)
    }

    fn and_expr(&mut self) -> PResult<Expr> {
        let mut e = self.not_expr()?;
        while self.eat_kw("and") {
            let r = self.not_expr()?;
            e = Expr::bin(BinOp::And, e, r);
        }
        Ok(e)
    }

    fn not_expr(&mut self) -> PResult<Expr> {
        if self.eat_kw("not") {
            let e = self.not_expr()?;
            Ok(Expr::Unary {
                op: UnOp::Not,
                expr: Box::new(e),
            })
        } else {
            self.cmp_expr()
        }
    }

    fn cmp_expr(&mut self) -> PResult<Expr> {
        let left = self.add_expr()?;
        if self.eat_kw("is") {
            let negated = self.eat_kw("not");
            self.expect_kw("null")?;
            return Ok(Expr::IsNull {
                expr: Box::new(left),
                negated,
            });
        }
        let mut in_negated = false;
        if self.is_kw("not")
            && self.toks.get(self.i + 1).map(|t| &t.tok) == Some(&Tok::Word("in".into()))
        {
            self.bump();
            in_negated = true;
        }
        if self.eat_kw("in") {
            self.expect(&Tok::LParen)?;
            if self.is_kw("select") {
                let q = self.select()?;
                self.expect(&Tok::RParen)?;
                return Ok(Expr::InSubquery {
                    expr: Box::new(left),
                    query: Box::new(q),
                    negated: in_negated,
                });
            }
            let mut list = vec![self.expr()?];
            while self.eat(&Tok::Comma) {
                list.push(self.expr()?);
            }
            self.expect(&Tok::RParen)?;
            return Ok(Expr::InList {
                expr: Box::new(left),
                list,
                negated: in_negated,
            });
        }
        let op = match self.peek() {
            Tok::Eq => BinOp::Eq,
            Tok::Ne => BinOp::Ne,
            Tok::Lt => BinOp::Lt,
            Tok::Le => BinOp::Le,
            Tok::Gt => BinOp::Gt,
            Tok::Ge => BinOp::Ge,
            _ => return Ok(left),
        };
        self.bump();
        let right = self.add_expr()?;
        Ok(Expr::bin(op, left, right))
    }

    fn add_expr(&mut self) -> PResult<Expr> {
        let mut e = self.mul_expr()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                _ => break,
            };
            self.bump();
            let r = self.mul_expr()?;
            e = Expr::bin(op, e, r);
        }
        Ok(e)
    }

    fn mul_expr(&mut self) -> PResult<Expr> {
        let mut e = self.unary_expr()?;
        loop {
            let op = match self.peek() {
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                _ => break,
            };
            self.bump();
            let r = self.unary_expr()?;
            e = Expr::bin(op, e, r);
        }
        Ok(e)
    }

    fn unary_expr(&mut self) -> PResult<Expr> {
        if self.eat(&Tok::Minus) {
            let e = self.unary_expr()?;
            return Ok(Expr::Unary {
                op: UnOp::Neg,
                expr: Box::new(e),
            });
        }
        if self.eat(&Tok::Plus) {
            return self.unary_expr();
        }
        self.atom()
    }

    fn atom(&mut self) -> PResult<Expr> {
        match self.peek().clone() {
            Tok::Int(n) => {
                self.bump();
                Ok(Expr::Literal(Value::BigInt(n)))
            }
            Tok::Float(f) => {
                self.bump();
                Ok(Expr::Literal(Value::Double(f)))
            }
            Tok::Str(s) => {
                self.bump();
                Ok(Expr::Literal(Value::Text(s)))
            }
            Tok::LParen => {
                self.bump();
                if self.is_kw("select") {
                    let q = self.select()?;
                    self.expect(&Tok::RParen)?;
                    return Ok(Expr::ScalarSubquery(Box::new(q)));
                }
                let e = self.expr()?;
                self.expect(&Tok::RParen)?;
                Ok(e)
            }
            Tok::Word(w) => match w.as_str() {
                "null" => {
                    self.bump();
                    Ok(Expr::Literal(Value::Null))
                }
                "true" => {
                    self.bump();
                    Ok(Expr::Literal(Value::Bool(true)))
                }
                "false" => {
                    self.bump();
                    Ok(Expr::Literal(Value::Bool(false)))
                }
                "case" => self.case_expr(),
                "count" | "sum" | "avg" | "min" | "max" => self.aggregate(&w),
                _ if !is_reserved(&w) => self.column_ref(),
                _ => self.err(format!("unexpected keyword '{w}' in expression")),
            },
            other => self.err(format!("unexpected {other:?} in expression")),
        }
    }

    fn column_ref(&mut self) -> PResult<Expr> {
        let first = self.ident()?;
        if self.eat(&Tok::Dot) {
            let name = self.ident()?;
            Ok(Expr::Column {
                table: Some(first),
                name,
            })
        } else {
            Ok(Expr::Column {
                table: None,
                name: first,
            })
        }
    }

    fn aggregate(&mut self, name: &str) -> PResult<Expr> {
        let func = match name {
            "count" => AggFunc::Count,
            "sum" => AggFunc::Sum,
            "avg" => AggFunc::Avg,
            "min" => AggFunc::Min,
            "max" => AggFunc::Max,
            _ => unreachable!(),
        };
        self.bump();
        self.expect(&Tok::LParen)?;
        if self.eat(&Tok::Star) {
            self.expect(&Tok::RParen)?;
            return Ok(Expr::Aggregate {
                func,
                arg: None,
                distinct: false,
            });
        }
        let distinct = self.eat_kw("distinct");
        let arg = self.expr()?;
        self.expect(&Tok::RParen)?;
        Ok(Expr::Aggregate {
            func,
            arg: Some(Box::new(arg)),
            distinct,
        })
    }

    fn case_expr(&mut self) -> PResult<Expr> {
        self.expect_kw("case")?;
        let operand = if self.is_kw("when") {
            None
        } else {
            Some(Box::new(self.expr()?))
        };
        let mut whens = Vec::new();
        while self.eat_kw("when") {
            let cond = self.expr()?;
            self.expect_kw("then")?;
            let val = self.expr()?;
            whens.push((cond, val));
        }
        let els = if self.eat_kw("else") {
            Some(Box::new(self.expr()?))
        } else {
            None
        };
        self.expect_kw("end")?;
        Ok(Expr::Case {
            operand,
            whens,
            els,
        })
    }
}

/// Reserved words that can't be bare identifiers. Kept deliberately small — SQL's
/// giant reserved list isn't needed for the freeze set.
fn is_reserved(w: &str) -> bool {
    matches!(
        w,
        "select"
            | "from"
            | "where"
            | "group"
            | "having"
            | "order"
            | "by"
            | "limit"
            | "and"
            | "or"
            | "not"
            | "is"
            | "null"
            | "in"
            | "as"
            | "join"
            | "left"
            | "inner"
            | "outer"
            | "on"
            | "case"
            | "when"
            | "then"
            | "else"
            | "end"
            | "insert"
            | "into"
            | "values"
            | "delete"
            | "update"
            | "set"
            | "drop"
            | "create"
            | "table"
            | "index"
            | "primary"
            | "key"
            | "distinct"
            | "asc"
            | "desc"
            | "begin"
            | "commit"
            | "rollback"
            | "true"
            | "false"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_create_table() {
        let s = parse_statement(
            "CREATE TABLE t (id BIGINT PRIMARY KEY, name VARCHAR(32) NOT NULL, x INT)",
        )
        .unwrap();
        match s {
            Stmt::CreateTable(ct) => {
                assert_eq!(ct.name, "t");
                assert_eq!(ct.columns.len(), 3);
                assert!(ct.columns[0].primary_key && ct.columns[0].not_null);
                assert_eq!(ct.columns[1].ty, ColumnType::Varchar(32));
                assert!(ct.columns[1].not_null);
                assert!(!ct.columns[2].not_null);
            }
            _ => panic!("expected CreateTable"),
        }
    }

    #[test]
    fn parse_insert() {
        let s = parse_statement("INSERT INTO t (id, name) VALUES (1, 'a'), (2, 'b')").unwrap();
        match s {
            Stmt::Insert(ins) => {
                assert_eq!(ins.table, "t");
                assert_eq!(ins.columns.as_ref().unwrap().len(), 2);
                assert_eq!(ins.rows.len(), 2);
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn parse_delete() {
        let s = parse_statement("DELETE FROM t WHERE id = 3").unwrap();
        match s {
            Stmt::Delete(d) => {
                assert_eq!(d.table, "t");
                assert!(d.filter.is_some());
            }
            _ => panic!("expected Delete"),
        }
        let all = parse_statement("DELETE FROM t").unwrap();
        assert!(matches!(all, Stmt::Delete(d) if d.filter.is_none()));
    }

    #[test]
    fn parse_drop() {
        assert!(matches!(
            parse_statement("DROP TABLE t").unwrap(),
            Stmt::DropTable(n) if n == "t"
        ));
        assert!(matches!(
            parse_statement("DROP INDEX ix").unwrap(),
            Stmt::DropIndex(n) if n == "ix"
        ));
    }

    #[test]
    fn parse_update() {
        let s = parse_statement("UPDATE t SET a = 1, b = a + 2 WHERE id = 3").unwrap();
        match s {
            Stmt::Update(u) => {
                assert_eq!(u.table, "t");
                assert_eq!(u.assignments.len(), 2);
                assert_eq!(u.assignments[0].0, "a");
                assert_eq!(u.assignments[1].0, "b");
                assert!(u.filter.is_some());
            }
            _ => panic!("expected Update"),
        }
    }

    #[test]
    fn expr_precedence() {
        let e = parse_expr("1 + 2 * 3 = 7").unwrap();
        match e {
            Expr::Binary {
                op: BinOp::Eq,
                left,
                ..
            } => match *left {
                Expr::Binary {
                    op: BinOp::Add,
                    right,
                    ..
                } => {
                    assert!(matches!(*right, Expr::Binary { op: BinOp::Mul, .. }));
                }
                _ => panic!("left should be +"),
            },
            _ => panic!("top should be ="),
        }
    }

    #[test]
    fn and_or_precedence() {
        let e = parse_expr("a OR b AND c").unwrap();
        assert!(matches!(e, Expr::Binary { op: BinOp::Or, .. }));
    }

    #[test]
    fn parse_select_full() {
        let s = parse_statement(
            "SELECT t.a, COUNT(*) AS n FROM t LEFT JOIN u ON t.a = u.b \
             WHERE t.a > 3 AND t.c IS NOT NULL GROUP BY t.a HAVING COUNT(*) > 1 \
             ORDER BY n DESC LIMIT 10",
        )
        .unwrap();
        match s {
            Stmt::Select(q) => {
                assert_eq!(q.items.len(), 2);
                let from = q.from.unwrap();
                assert_eq!(from.joins.len(), 1);
                assert_eq!(from.joins[0].0, JoinKind::Left);
                assert!(q.filter.is_some());
                assert_eq!(q.group_by.len(), 1);
                assert!(q.having.is_some());
                assert_eq!(q.order_by.len(), 1);
                assert!(!q.order_by[0].asc);
                assert_eq!(q.limit, Some(10));
            }
            _ => panic!("expected Select"),
        }
    }

    #[test]
    fn parse_case_and_in() {
        parse_expr("CASE WHEN x > 0 THEN 'pos' WHEN x < 0 THEN 'neg' ELSE 'zero' END").unwrap();
        parse_expr("x IN (1, 2, 3)").unwrap();
        parse_expr("x NOT IN (SELECT a FROM t)").unwrap();
        parse_expr("(SELECT MAX(a) FROM t)").unwrap();
    }

    #[test]
    fn null_semantics_parse() {
        parse_expr("a IS NULL").unwrap();
        parse_expr("a IS NOT NULL").unwrap();
        parse_expr("NOT a = b").unwrap();
    }
}
