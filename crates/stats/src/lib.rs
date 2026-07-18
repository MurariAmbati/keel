//! Statistics and cardinality estimation (§6.3–6.4).
//!
//! `analyze` samples a table into per-column statistics: null fraction, distinct
//! count (via **HyperLogLog**), min/max, and an equi-depth **histogram** over the
//! numeric columns. `estimate_selectivity` turns a WHERE predicate into an
//! expected surviving fraction (histogram interpolation for ranges, `1/NDV` for
//! equality, independence across `AND`). The headline optimizer number is the
//! **q-error** — `max(est/act, act/est)` per query — which is what makes "how
//! good is my estimator" quantitative (§6.4, the JOB-paper worldview).

use keel_sql::{BinOp, Expr, UnOp};
use keel_types::{Schema, Value};

/// A small HyperLogLog for estimating the number of distinct values.
#[derive(Clone, Debug)]
pub struct Hll {
    registers: Vec<u8>,
    p: u32,
}

impl Hll {
    pub fn new() -> Self {
        let p = 12;
        Hll {
            registers: vec![0; 1 << p],
            p,
        }
    }

    pub fn add_hash(&mut self, hash: u64) {
        let idx = (hash >> (64 - self.p)) as usize;
        let w = (hash << self.p) | (1u64 << (self.p - 1));
        let rank = (w.leading_zeros() + 1) as u8;
        if rank > self.registers[idx] {
            self.registers[idx] = rank;
        }
    }

    pub fn estimate(&self) -> u64 {
        let m = self.registers.len() as f64;
        let sum: f64 = self.registers.iter().map(|&r| 2f64.powi(-(r as i32))).sum();
        let alpha = 0.7213 / (1.0 + 1.079 / m);
        let raw = alpha * m * m / sum;
        let zeros = self.registers.iter().filter(|&&r| r == 0).count() as f64;
        let est = if raw <= 2.5 * m && zeros > 0.0 {
            m * (m / zeros).ln()
        } else {
            raw
        };
        est.round().max(1.0) as u64
    }
}
impl Default for Hll {
    fn default() -> Self {
        Self::new()
    }
}

fn hash_value(v: &Value) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let mut mix = |byte: u8| {
        h ^= byte as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    match v {
        Value::Null => mix(0),
        Value::Bool(b) => {
            mix(1);
            mix(*b as u8);
        }
        Value::Int(i) => {
            mix(2);
            i.to_le_bytes().iter().for_each(|&b| mix(b));
        }
        Value::BigInt(i) => {
            mix(3);
            i.to_le_bytes().iter().for_each(|&b| mix(b));
        }
        Value::Double(d) => {
            mix(4);
            d.to_bits().to_le_bytes().iter().for_each(|&b| mix(b));
        }
        Value::Text(s) => {
            mix(5);
            s.bytes().for_each(&mut mix);
        }
    }
    let mut z = h;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// The numeric value of `v` for histograms/min/max, or `None` for non-numeric.
fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::BigInt(i) => Some(*i as f64),
        Value::Double(d) => Some(*d),
        Value::Bool(b) => Some(*b as i64 as f64),
        _ => None,
    }
}

const HIST_BUCKETS: usize = 32;

#[derive(Clone, Debug)]
pub struct ColumnStats {
    pub null_frac: f64,
    pub ndv: u64,
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// Equi-depth boundaries (`HIST_BUCKETS + 1` of them) over non-null numeric
    /// values; `None` for non-numeric columns.
    pub hist: Option<Vec<f64>>,
}

#[derive(Clone, Debug)]
pub struct TableStats {
    pub row_count: u64,
    pub columns: Vec<ColumnStats>,
}

/// Compute statistics over a table's rows (a full `ANALYZE`; sampling is a later
/// refinement).
pub fn analyze(schema: &Schema, rows: &[Vec<Value>]) -> TableStats {
    let ncols = schema.columns.len();
    let mut columns = Vec::with_capacity(ncols);
    for c in 0..ncols {
        let mut nulls = 0u64;
        let mut hll = Hll::new();
        let mut nums: Vec<f64> = Vec::new();
        for row in rows {
            let v = &row[c];
            if v.is_null() {
                nulls += 1;
            } else {
                hll.add_hash(hash_value(v));
                if let Some(f) = as_f64(v) {
                    nums.push(f);
                }
            }
        }
        let n = rows.len().max(1) as f64;
        let (min, max, hist) = if !nums.is_empty() {
            nums.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let min = nums[0];
            let max = *nums.last().unwrap();
            let hist = equi_depth(&nums, HIST_BUCKETS);
            (Some(min), Some(max), Some(hist))
        } else {
            (None, None, None)
        };
        columns.push(ColumnStats {
            null_frac: nulls as f64 / n,
            ndv: if rows.is_empty() { 0 } else { hll.estimate() },
            min,
            max,
            hist,
        });
    }
    TableStats {
        row_count: rows.len() as u64,
        columns,
    }
}

/// Equi-depth histogram boundaries over sorted numeric values.
fn equi_depth(sorted: &[f64], buckets: usize) -> Vec<f64> {
    let n = sorted.len();
    let b = buckets.min(n.max(1));
    let mut bounds = Vec::with_capacity(b + 1);
    for i in 0..=b {
        let idx = (i * (n - 1)) / b.max(1);
        bounds.push(sorted[idx.min(n - 1)]);
    }
    bounds
}

/// Fraction of the (non-null numeric) values strictly below `v`, via bucket
/// interpolation. In `[0, 1]`.
fn frac_below(hist: &[f64], v: f64) -> f64 {
    if hist.len() < 2 {
        return if v > hist.first().copied().unwrap_or(0.0) {
            1.0
        } else {
            0.0
        };
    }
    let n = hist.len() - 1;
    if v <= hist[0] {
        return 0.0;
    }
    if v >= hist[n] {
        return 1.0;
    }
    for i in 0..n {
        if v < hist[i + 1] {
            let lo = hist[i];
            let hi = hist[i + 1];
            let within = if hi > lo { (v - lo) / (hi - lo) } else { 0.0 };
            return (i as f64 + within) / n as f64;
        }
    }
    1.0
}

const DEFAULT_RANGE: f64 = 0.33;
const DEFAULT_OTHER: f64 = 0.5;

/// Estimate the fraction of rows a predicate keeps (as a WHERE filter), in
/// `[0, 1]`. Uses column stats for comparisons on `column <op> literal`,
/// independence for `AND`, inclusion–exclusion for `OR`.
pub fn estimate_selectivity(pred: &Expr, schema: &Schema, stats: &TableStats) -> f64 {
    let s = estimate(pred, schema, stats);
    s.clamp(0.0, 1.0)
}

fn col_index(schema: &Schema, e: &Expr) -> Option<usize> {
    if let Expr::Column { name, .. } = e {
        schema.column_index(name)
    } else {
        None
    }
}

fn estimate(pred: &Expr, schema: &Schema, stats: &TableStats) -> f64 {
    match pred {
        Expr::Binary {
            op: BinOp::And,
            left,
            right,
        } => estimate(left, schema, stats) * estimate(right, schema, stats),
        Expr::Binary {
            op: BinOp::Or,
            left,
            right,
        } => {
            let a = estimate(left, schema, stats);
            let b = estimate(right, schema, stats);
            a + b - a * b
        }
        Expr::Unary {
            op: UnOp::Not,
            expr,
        } => 1.0 - estimate(expr, schema, stats),
        Expr::IsNull { expr, negated } => match col_index(schema, expr) {
            Some(ci) => {
                let nf = stats.columns[ci].null_frac;
                if *negated {
                    1.0 - nf
                } else {
                    nf
                }
            }
            None => DEFAULT_OTHER,
        },
        Expr::InList {
            expr,
            list,
            negated,
        } => match col_index(schema, expr) {
            Some(ci) => {
                let cs = &stats.columns[ci];
                let each = (1.0 - cs.null_frac) / cs.ndv.max(1) as f64;
                let sel = (each * list.len() as f64).min(1.0 - cs.null_frac);
                if *negated {
                    (1.0 - cs.null_frac) - sel
                } else {
                    sel
                }
            }
            None => DEFAULT_OTHER,
        },
        Expr::Binary { op, left, right }
            if matches!(
                op,
                BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
            ) =>
        {
            comparison_sel(*op, left, right, schema, stats)
        }
        Expr::Literal(Value::Bool(true)) => 1.0,
        Expr::Literal(Value::Bool(false)) | Expr::Literal(Value::Null) => 0.0,
        _ => DEFAULT_OTHER,
    }
}

fn comparison_sel(
    op: BinOp,
    left: &Expr,
    right: &Expr,
    schema: &Schema,
    stats: &TableStats,
) -> f64 {
    let (ci, op, lit) = match (col_index(schema, left), col_index(schema, right)) {
        (Some(ci), None) => (ci, op, as_lit(right)),
        (None, Some(ci)) => (ci, flip(op), as_lit(left)),
        _ => return DEFAULT_RANGE,
    };
    let cs = &stats.columns[ci];
    let nonnull = 1.0 - cs.null_frac;

    match op {
        BinOp::Eq => nonnull / cs.ndv.max(1) as f64,
        BinOp::Ne => nonnull * (1.0 - 1.0 / cs.ndv.max(1) as f64),
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            let f = match (lit.as_ref().and_then(as_f64), &cs.hist) {
                (Some(v), Some(hist)) => frac_below(hist, v),
                _ => return nonnull * DEFAULT_RANGE,
            };
            let below = f;
            let sel = match op {
                BinOp::Lt | BinOp::Le => below,
                _ => 1.0 - below,
            };
            nonnull * sel
        }
        _ => DEFAULT_RANGE,
    }
}

fn as_lit(e: &Expr) -> Option<Value> {
    if let Expr::Literal(v) = e {
        Some(v.clone())
    } else {
        None
    }
}
fn flip(op: BinOp) -> BinOp {
    match op {
        BinOp::Lt => BinOp::Gt,
        BinOp::Le => BinOp::Ge,
        BinOp::Gt => BinOp::Lt,
        BinOp::Ge => BinOp::Le,
        other => other,
    }
}

/// q-error between an estimate and the actual (both smoothed by 1 to avoid
/// division by zero): `max((est+1)/(act+1), (act+1)/(est+1))`, always `>= 1`.
pub fn q_error(estimated: f64, actual: f64) -> f64 {
    let e = estimated.max(0.0) + 1.0;
    let a = actual.max(0.0) + 1.0;
    (e / a).max(a / e)
}

/// Access-path advice: whether an index scan is expected to beat a full scan for
/// the given selectivity, under a simple cost model (an index scan touches
/// `sel * rows` tuples plus per-tuple random-fetch overhead; a seq scan touches
/// every page). Below the crossover selectivity, prefer the index.
pub fn prefer_index_scan(selectivity: f64, crossover: f64) -> bool {
    selectivity <= crossover
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_sql::gen::{random_predicate, random_value, Gen};
    use keel_sql::refengine::MemDb;
    use keel_sql::{parse_statement, Stmt};
    use keel_types::{ColumnDef, ColumnType};

    fn schema() -> Schema {
        Schema::new(vec![
            ColumnDef::new("a", ColumnType::Int, false),
            ColumnDef::new("b", ColumnType::Int, false),
            ColumnDef::new("f", ColumnType::Double, false),
            ColumnDef::new("s", ColumnType::Varchar(4), false),
        ])
    }

    #[test]
    fn hll_estimates_distinct_count() {
        let mut hll = Hll::new();
        for i in 0..10_000u64 {
            hll.add_hash(hash_value(&Value::BigInt(i as i64)));
        }
        let est = hll.estimate();
        let err = (est as f64 - 10_000.0).abs() / 10_000.0;
        assert!(
            err < 0.06,
            "HLL estimate {est} too far from 10000 ({err:.3})"
        );
    }

    #[test]
    fn point_selectivity_is_reasonable() {
        let sch = schema();
        let rows: Vec<Vec<Value>> = (0..1000)
            .map(|i| {
                vec![
                    Value::Int(i % 10),
                    Value::Int(0),
                    Value::Double(0.0),
                    Value::Text("x".into()),
                ]
            })
            .collect();
        let stats = analyze(&sch, &rows);
        let p = parse_statement("SELECT * FROM t WHERE a = 3").unwrap();
        if let Stmt::Select(q) = p {
            let sel = estimate_selectivity(&q.filter.unwrap(), &sch, &stats);
            assert!((sel - 0.1).abs() < 0.03, "eq selectivity {sel} not ~0.1");
        }
    }

    #[test]
    fn q_error_distribution_is_bounded() {
        let cols: Vec<(String, ColumnType)> = vec![
            ("a".into(), ColumnType::Int),
            ("b".into(), ColumnType::Int),
            ("f".into(), ColumnType::Double),
            ("s".into(), ColumnType::Varchar(4)),
        ];
        let sch = schema();
        let mut qerrs: Vec<f64> = Vec::new();
        for seed in 0..60u64 {
            let mut g = Gen::new(seed);
            let rows: Vec<Vec<Value>> = (0..800)
                .map(|_| {
                    cols.iter()
                        .map(|(_, t)| random_value(&mut g, *t, false))
                        .collect()
                })
                .collect();
            let stats = analyze(&sch, &rows);
            let mut mem = MemDb::new();
            mem.install_table("t", sch.clone(), rows.clone());

            for _ in 0..15 {
                let pred = random_predicate(&mut g, &cols, 2);
                let est_sel = estimate_selectivity(&pred, &sch, &stats);
                let est_card = est_sel * stats.row_count as f64;

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
                    filter: Some(pred.clone()),
                    group_by: Vec::new(),
                    having: None,
                    order_by: Vec::new(),
                    limit: None,
                };
                let actual = mem.query(&q).unwrap().rows.len() as f64;
                qerrs.push(q_error(est_card, actual));
            }
        }
        qerrs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = qerrs[qerrs.len() / 2];
        let p90 = qerrs[qerrs.len() * 9 / 10];
        eprintln!("q-error: median={median:.2} p90={p90:.2} n={}", qerrs.len());
        assert!(median < 3.0, "median q-error {median:.2} too high");
    }

    #[test]
    fn access_path_crossover() {
        assert!(prefer_index_scan(0.01, 0.05));
        assert!(!prefer_index_scan(0.5, 0.05));
    }
}
