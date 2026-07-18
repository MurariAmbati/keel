//! Schema-aware random generation and metamorphic oracles (§7.2).
//!
//! The generator builds type-correct predicates by construction; the tests apply
//! **Ternary Logic Partitioning** (Rigger & Su, OOPSLA'20): for any predicate
//! `p`, every row falls into exactly one of `p` (TRUE), `NOT p` (FALSE), or
//! `p IS NULL` (UNKNOWN), so
//!
//! ```text
//! SELECT * FROM t  ==  (SELECT * FROM t WHERE p)
//!               UNION ALL (SELECT * FROM t WHERE NOT p)
//!               UNION ALL (SELECT * FROM t WHERE p IS NULL)
//! ```
//!
//! as multisets. A bug in the three-valued logic (NULL comparisons, `NOT`, `AND`,
//! `OR`, `IN`) breaks the identity — TLP is the standing adversary the design
//! points at the NULL-semantics bug farm.

use keel_types::{ColumnType, Value};

use crate::ast::*;

/// A tiny deterministic RNG local to this module (so `keel-sql` needs no dep).
pub struct Gen {
    s: u64,
}
impl Gen {
    pub fn new(seed: u64) -> Self {
        Gen {
            s: seed ^ 0x9E37_79B9_7F4A_7C15,
        }
    }
    fn next(&mut self) -> u64 {
        self.s = self.s.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.s;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }
    fn chance(&mut self, num: u64, den: u64) -> bool {
        self.below(den) < num
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len() as u64) as usize]
    }
}

/// A random value of a column type, `NULL` with probability ~1/6 unless
/// `allow_null` is false. Small ranges/alphabets to force collisions and edges.
pub fn random_value(g: &mut Gen, ty: ColumnType, allow_null: bool) -> Value {
    if allow_null && g.chance(1, 6) {
        return Value::Null;
    }
    match ty {
        ColumnType::Bool => Value::Bool(g.below(2) == 0),
        ColumnType::Int => Value::Int(g.below(7) as i32 - 3),
        ColumnType::BigInt => Value::BigInt(g.below(7) as i64 - 3),
        ColumnType::Double => Value::Double(g.below(7) as f64 - 3.0),
        ColumnType::Varchar(_) => {
            let alpha = [b'a', b'b', b'c'];
            let len = g.below(3);
            let s: String = (0..len).map(|_| *g.pick(&alpha) as char).collect();
            Value::Text(s)
        }
    }
}

/// A literal expression for a value.
fn lit(v: Value) -> Expr {
    Expr::Literal(v)
}

/// A random, type-correct boolean predicate over `cols` (name, type).
pub fn random_predicate(g: &mut Gen, cols: &[(String, ColumnType)], depth: u32) -> Expr {
    if depth == 0 || g.chance(1, 2) {
        atom(g, cols)
    } else {
        match g.below(3) {
            0 => Expr::bin(
                BinOp::And,
                random_predicate(g, cols, depth - 1),
                random_predicate(g, cols, depth - 1),
            ),
            1 => Expr::bin(
                BinOp::Or,
                random_predicate(g, cols, depth - 1),
                random_predicate(g, cols, depth - 1),
            ),
            _ => Expr::Unary {
                op: UnOp::Not,
                expr: Box::new(random_predicate(g, cols, depth - 1)),
            },
        }
    }
}

fn atom(g: &mut Gen, cols: &[(String, ColumnType)]) -> Expr {
    let (name, ty) = g.pick(cols).clone();
    match g.below(4) {
        0 => {
            let op = *g.pick(&[
                BinOp::Eq,
                BinOp::Ne,
                BinOp::Lt,
                BinOp::Le,
                BinOp::Gt,
                BinOp::Ge,
            ]);
            Expr::bin(op, Expr::col(&name), lit(random_value(g, ty, true)))
        }
        1 => {
            let same: Vec<_> = cols.iter().filter(|(_, t)| *t == ty).collect();
            let (other, _) = g.pick(&same);
            let op = *g.pick(&[BinOp::Eq, BinOp::Ne, BinOp::Lt, BinOp::Gt]);
            Expr::bin(op, Expr::col(&name), Expr::col(other))
        }
        2 => Expr::IsNull {
            expr: Box::new(Expr::col(&name)),
            negated: g.below(2) == 0,
        },
        _ => {
            let list = (0..1 + g.below(3))
                .map(|_| lit(random_value(g, ty, true)))
                .collect();
            Expr::InList {
                expr: Box::new(Expr::col(&name)),
                list,
                negated: g.below(2) == 0,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refengine::{MemDb, Row};
    use keel_types::{ColumnDef, Schema};
    use std::cmp::Ordering;

    fn columns() -> Vec<(String, ColumnType)> {
        vec![
            ("id".into(), ColumnType::BigInt),
            ("a".into(), ColumnType::Int),
            ("b".into(), ColumnType::Int),
            ("f".into(), ColumnType::Double),
            ("s".into(), ColumnType::Varchar(4)),
            ("bo".into(), ColumnType::Bool),
        ]
    }

    fn build_db(g: &mut Gen, cols: &[(String, ColumnType)], nrows: usize) -> MemDb {
        let schema = Schema::new(
            cols.iter()
                .map(|(n, t)| ColumnDef::new(n.clone(), *t, n == "id"))
                .collect(),
        );
        let mut rows: Vec<Row> = Vec::new();
        for i in 0..nrows {
            let mut row = Vec::new();
            for (name, ty) in cols {
                if name == "id" {
                    row.push(Value::BigInt(i as i64));
                } else {
                    row.push(random_value(g, *ty, true));
                }
            }
            rows.push(row);
        }
        let mut db = MemDb::new();
        db.install_table("t", schema, rows);
        db
    }

    fn select_all(db: &MemDb, filter: Option<Expr>) -> Vec<Row> {
        let q = Select {
            distinct: false,
            items: vec![SelectItem::Wildcard],
            from: Some(FromClause {
                first: TableRef {
                    table: "t".into(),
                    alias: None,
                },
                joins: Vec::new(),
            }),
            filter,
            group_by: Vec::new(),
            having: None,
            order_by: Vec::new(),
            limit: None,
        };
        db.query(&q).unwrap().rows
    }

    fn sorted(mut rows: Vec<Row>) -> Vec<Row> {
        rows.sort_by(|a, b| {
            for (x, y) in a.iter().zip(b) {
                let o = x.total_cmp(y);
                if o != Ordering::Equal {
                    return o;
                }
            }
            Ordering::Equal
        });
        rows
    }

    #[test]
    fn tlp_partition_identity_holds() {
        let cols = columns();
        for seed in 0..200u64 {
            let mut g = Gen::new(seed);
            let db = build_db(&mut g, &cols, 40);
            for _ in 0..10 {
                let p = random_predicate(&mut g, &cols, 3);
                let base = select_all(&db, None);
                let t = select_all(&db, Some(p.clone()));
                let f = select_all(
                    &db,
                    Some(Expr::Unary {
                        op: UnOp::Not,
                        expr: Box::new(p.clone()),
                    }),
                );
                let u = select_all(
                    &db,
                    Some(Expr::IsNull {
                        expr: Box::new(p.clone()),
                        negated: false,
                    }),
                );

                let mut union = t;
                union.extend(f);
                union.extend(u);

                assert_eq!(
                    sorted(base),
                    sorted(union),
                    "TLP identity broken for seed {seed}, predicate {p:?}"
                );
            }
        }
    }
}
