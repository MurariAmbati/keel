use keel_sql::{BinOp, Expr, UnOp};
use keel_types::Value;

pub const BATCH: usize = 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Batch {
    pub names: Vec<(String, String)>,
    pub cols: Vec<Vec<Value>>,
    pub len: usize,
}

impl Batch {
    pub fn from_rows(names: Vec<(String, String)>, rows: &[Vec<Value>]) -> Batch {
        let ncols = names.len();
        let mut cols = vec![Vec::with_capacity(rows.len()); ncols];
        for row in rows {
            for (c, v) in row.iter().enumerate() {
                cols[c].push(v.clone());
            }
        }
        Batch {
            names,
            cols,
            len: rows.len(),
        }
    }

    pub fn to_rows(&self) -> Vec<Vec<Value>> {
        (0..self.len)
            .map(|r| self.cols.iter().map(|c| c[r].clone()).collect())
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct VError(pub String);
fn err<T>(m: impl Into<String>) -> Result<T, VError> {
    Err(VError(m.into()))
}
type R<T> = Result<T, VError>;

fn col_index(names: &[(String, String)], name: &str) -> Option<usize> {
    names.iter().position(|(_, c)| c == name)
}

pub fn eval_vec(e: &Expr, batch: &Batch) -> R<Vec<Value>> {
    match e {
        Expr::Literal(v) => Ok(vec![v.clone(); batch.len]),
        Expr::Column { name, .. } => match col_index(&batch.names, name) {
            Some(i) => Ok(batch.cols[i].clone()),
            None => err(format!("unknown column '{name}'")),
        },
        Expr::Unary { op, expr } => {
            let v = eval_vec(expr, batch)?;
            Ok(v.iter()
                .map(|x| match op {
                    UnOp::Not => not3(to_bool3(x)),
                    UnOp::Neg => neg(x),
                })
                .collect())
        }
        Expr::IsNull { expr, negated } => {
            let v = eval_vec(expr, batch)?;
            Ok(v.iter()
                .map(|x| Value::Bool(x.is_null() != *negated))
                .collect())
        }
        Expr::Binary { op, left, right } => {
            let l = eval_vec(left, batch)?;
            let r = eval_vec(right, batch)?;
            Ok((0..batch.len).map(|i| binop(*op, &l[i], &r[i])).collect())
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let v = eval_vec(expr, batch)?;
            let lists: Vec<Vec<Value>> =
                list.iter().map(|e| eval_vec(e, batch)).collect::<R<_>>()?;
            Ok((0..batch.len)
                .map(|i| {
                    let items = lists.iter().map(|c| &c[i]);
                    in3(&v[i], items, *negated)
                })
                .collect())
        }
        _ => err("expression form not supported by the vectorized path"),
    }
}

pub fn filter(pred: &Expr, batch: &Batch) -> R<Batch> {
    let mask = eval_vec(pred, batch)?;
    let sel: Vec<usize> = (0..batch.len)
        .filter(|&i| matches!(mask[i], Value::Bool(true)))
        .collect();
    Ok(select(batch, &sel))
}

pub fn project(exprs: &[Expr], out_names: &[(String, String)], batch: &Batch) -> R<Batch> {
    let cols: Vec<Vec<Value>> = exprs.iter().map(|e| eval_vec(e, batch)).collect::<R<_>>()?;
    Ok(Batch {
        names: out_names.to_vec(),
        cols,
        len: batch.len,
    })
}

fn select(batch: &Batch, sel: &[usize]) -> Batch {
    let cols = batch
        .cols
        .iter()
        .map(|c| sel.iter().map(|&i| c[i].clone()).collect())
        .collect();
    Batch {
        names: batch.names.clone(),
        cols,
        len: sel.len(),
    }
}

fn to_bool3(v: &Value) -> Option<bool> {
    match v {
        Value::Null => None,
        Value::Bool(b) => Some(*b),
        _ => Some(true),
    }
}
fn not3(b: Option<bool>) -> Value {
    match b {
        None => Value::Null,
        Some(b) => Value::Bool(!b),
    }
}
fn and3(l: &Value, r: &Value) -> Value {
    match (to_bool3(l), to_bool3(r)) {
        (Some(false), _) | (_, Some(false)) => Value::Bool(false),
        (Some(true), Some(true)) => Value::Bool(true),
        _ => Value::Null,
    }
}
fn or3(l: &Value, r: &Value) -> Value {
    match (to_bool3(l), to_bool3(r)) {
        (Some(true), _) | (_, Some(true)) => Value::Bool(true),
        (Some(false), Some(false)) => Value::Bool(false),
        _ => Value::Null,
    }
}
fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::BigInt(i) => Some(*i as f64),
        Value::Double(d) => Some(*d),
        _ => None,
    }
}
fn neg(v: &Value) -> Value {
    match v {
        Value::Int(i) => Value::Int(-i),
        Value::BigInt(i) => Value::BigInt(-i),
        Value::Double(d) => Value::Double(-d),
        _ => Value::Null,
    }
}

fn binop(op: BinOp, l: &Value, r: &Value) -> Value {
    match op {
        BinOp::And => and3(l, r),
        BinOp::Or => or3(l, r),
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => arith(op, l, r),
        _ => compare(op, l, r),
    }
}

fn arith(op: BinOp, l: &Value, r: &Value) -> Value {
    if l.is_null() || r.is_null() {
        return Value::Null;
    }
    let (lf, rf) = match (num(l), num(r)) {
        (Some(a), Some(b)) => (a, b),
        _ => return Value::Null,
    };
    let float = matches!(l, Value::Double(_)) || matches!(r, Value::Double(_)) || op == BinOp::Div;
    if float {
        let res = match op {
            BinOp::Add => lf + rf,
            BinOp::Sub => lf - rf,
            BinOp::Mul => lf * rf,
            BinOp::Div => {
                if rf == 0.0 {
                    return Value::Null;
                }
                lf / rf
            }
            _ => unreachable!(),
        };
        Value::Double(res)
    } else {
        let (a, b) = (lf as i64, rf as i64);
        let res = match op {
            BinOp::Add => a.wrapping_add(b),
            BinOp::Sub => a.wrapping_sub(b),
            BinOp::Mul => a.wrapping_mul(b),
            _ => unreachable!(),
        };
        Value::BigInt(res)
    }
}

fn compare(op: BinOp, l: &Value, r: &Value) -> Value {
    if l.is_null() || r.is_null() {
        return Value::Null;
    }
    let ord = match (num(l), num(r)) {
        (Some(a), Some(b)) => a.partial_cmp(&b),
        _ => match (l, r) {
            (Value::Text(a), Value::Text(b)) => Some(a.cmp(b)),
            (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
            _ => None,
        },
    };
    let Some(ord) = ord else { return Value::Null };
    use std::cmp::Ordering::*;
    Value::Bool(match op {
        BinOp::Eq => ord == Equal,
        BinOp::Ne => ord != Equal,
        BinOp::Lt => ord == Less,
        BinOp::Le => ord != Greater,
        BinOp::Gt => ord == Greater,
        BinOp::Ge => ord != Less,
        _ => false,
    })
}

fn in3<'a>(v: &Value, items: impl Iterator<Item = &'a Value>, negated: bool) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    let mut saw_null = false;
    for it in items {
        if it.is_null() {
            saw_null = true;
            continue;
        }
        if matches!(compare(BinOp::Eq, v, it), Value::Bool(true)) {
            return Value::Bool(!negated);
        }
    }
    if saw_null {
        Value::Null
    } else {
        Value::Bool(negated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_sql::gen::{random_predicate, random_value, Gen};
    use keel_sql::refengine::MemDb;
    use keel_types::{ColumnDef, ColumnType, Schema};

    fn cols() -> Vec<(String, ColumnType)> {
        vec![
            ("a".into(), ColumnType::Int),
            ("b".into(), ColumnType::Int),
            ("f".into(), ColumnType::Double),
            ("s".into(), ColumnType::Varchar(4)),
            ("bo".into(), ColumnType::Bool),
        ]
    }

    #[test]
    fn batch_roundtrip() {
        let names = vec![("t".into(), "a".into()), ("t".into(), "b".into())];
        let rows = vec![
            vec![Value::Int(1), Value::Int(2)],
            vec![Value::Int(3), Value::Null],
        ];
        let b = Batch::from_rows(names, &rows);
        assert_eq!(b.len, 2);
        assert_eq!(b.to_rows(), rows);
    }

    #[test]
    fn vectorized_filter_matches_reference_engine() {
        let cols = cols();
        let schema = Schema::new(
            cols.iter()
                .map(|(n, t)| ColumnDef::new(n.clone(), *t, false))
                .collect(),
        );
        let names: Vec<(String, String)> =
            cols.iter().map(|(n, _)| ("t".into(), n.clone())).collect();

        for seed in 0..100u64 {
            let mut g = Gen::new(seed);
            let rows: Vec<Vec<Value>> = (0..300)
                .map(|_| {
                    cols.iter()
                        .map(|(_, t)| random_value(&mut g, *t, true))
                        .collect()
                })
                .collect();
            let batch = Batch::from_rows(names.clone(), &rows);

            let mut mem = MemDb::new();
            mem.install_table("t", schema.clone(), rows.clone());

            for _ in 0..12 {
                let p = random_predicate(&mut g, &cols, 3);
                let got = filter(&p, &batch).unwrap().to_rows();
                let q = keel_sql::Select {
                    distinct: false,
                    items: vec![keel_sql::SelectItem::Wildcard],
                    from: Some(keel_sql::FromClause {
                        first: keel_sql::TableRef {
                            table: "t".into(),
                            alias: None,
                        },
                        joins: Vec::new(),
                    }),
                    filter: Some(p.clone()),
                    group_by: Vec::new(),
                    having: None,
                    order_by: Vec::new(),
                    limit: None,
                };
                let want = mem.query(&q).unwrap().rows;
                assert_eq!(got, want, "seed {seed}: vectorized != row engine for {p:?}");
            }
        }
    }

    #[test]
    fn vectorized_project_arithmetic() {
        let names = vec![("t".into(), "a".into()), ("t".into(), "b".into())];
        let rows = vec![
            vec![Value::Int(3), Value::Int(4)],
            vec![Value::Int(10), Value::Null],
        ];
        let b = Batch::from_rows(names, &rows);
        let e = Expr::bin(BinOp::Add, Expr::col("a"), Expr::col("b"));
        let out = eval_vec(&e, &b).unwrap();
        assert_eq!(out, vec![Value::BigInt(7), Value::Null]);
    }
}
