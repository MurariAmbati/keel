//! A streaming (Volcano) executor — an *independent* SELECT implementation, so
//! the storage engine has two engines to differ against (§7.1): this one and the
//! materializing reference engine.
//!
//! Pull-based iterator operators (Scan → Filter → Project → Distinct → Sort →
//! Limit) run tuple-at-a-time; `Sort`/`Distinct` are the blocking operators.
//! Multi-table queries run through a left-deep **hash join** (`try_stream_join`,
//! inner + left equijoins); grouped/aggregated queries run through a **hash
//! aggregate** (`run_aggregate` — GROUP BY / HAVING and the five aggregates), which
//! implements grouping and reduction here but reuses the shared `eval_public` for
//! every scalar sub-expression. Single-table, join, and aggregate paths share the
//! same projection/order/limit tail (`finish`). The planner is deliberately
//! conservative: subqueries, ORDER BY over a non-projected column, and
//! non-equijoin/straddling ON clauses return `None`, and `Database::select` falls
//! back to the materializing reference engine. Every path is exercised by the
//! differential campaign.

use std::cmp::Ordering;

use keel_sql::refengine::{column_label, eval_public, is_subquery_free, ExecError, ResultSet, Row};
use keel_sql::{AggFunc, Expr, Select, SelectItem};
use keel_types::{Schema, Value};

type Sch = Vec<(String, String)>;

/// A pull-based operator.
trait Op {
    fn next(&mut self) -> Option<Result<Row, ExecError>>;
}

struct Scan {
    it: std::vec::IntoIter<Row>,
}
impl Op for Scan {
    fn next(&mut self) -> Option<Result<Row, ExecError>> {
        self.it.next().map(Ok)
    }
}

struct Filter {
    child: Box<dyn Op>,
    pred: Expr,
    schema: Sch,
}
impl Op for Filter {
    fn next(&mut self) -> Option<Result<Row, ExecError>> {
        loop {
            match self.child.next()? {
                Ok(row) => match eval_public(&self.pred, &self.schema, &row) {
                    Ok(Value::Bool(true)) => return Some(Ok(row)),
                    Ok(_) => continue,
                    Err(e) => return Some(Err(e)),
                },
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

struct Project {
    child: Box<dyn Op>,
    exprs: Vec<Expr>,
    schema: Sch,
}
impl Op for Project {
    fn next(&mut self) -> Option<Result<Row, ExecError>> {
        match self.child.next()? {
            Ok(row) => Some(
                self.exprs
                    .iter()
                    .map(|e| eval_public(e, &self.schema, &row))
                    .collect::<Result<Row, ExecError>>(),
            ),
            Err(e) => Some(Err(e)),
        }
    }
}

struct Limit {
    child: Box<dyn Op>,
    remaining: usize,
}
impl Op for Limit {
    fn next(&mut self) -> Option<Result<Row, ExecError>> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        self.child.next()
    }
}

/// Blocking: drain the child, dedup preserving first-seen order.
struct Distinct {
    src: std::vec::IntoIter<Row>,
    done: bool,
    child: Option<Box<dyn Op>>,
}
impl Op for Distinct {
    fn next(&mut self) -> Option<Result<Row, ExecError>> {
        if !self.done {
            let mut child = self.child.take().unwrap();
            let mut seen: std::collections::BTreeSet<Vec<OrdVal>> =
                std::collections::BTreeSet::new();
            let mut out = Vec::new();
            while let Some(r) = child.next() {
                match r {
                    Ok(row) => {
                        let key: Vec<OrdVal> = row.iter().cloned().map(OrdVal).collect();
                        if seen.insert(key) {
                            out.push(row);
                        }
                    }
                    Err(e) => return Some(Err(e)),
                }
            }
            self.src = out.into_iter();
            self.done = true;
        }
        self.src.next().map(Ok)
    }
}

/// Blocking: drain the child, sort by output-column keys.
struct Sort {
    src: std::vec::IntoIter<Row>,
    done: bool,
    child: Option<Box<dyn Op>>,
    keys: Vec<(usize, bool)>,
}
impl Op for Sort {
    fn next(&mut self) -> Option<Result<Row, ExecError>> {
        if !self.done {
            let mut child = self.child.take().unwrap();
            let mut rows = Vec::new();
            while let Some(r) = child.next() {
                match r {
                    Ok(row) => rows.push(row),
                    Err(e) => return Some(Err(e)),
                }
            }
            let keys = self.keys.clone();
            rows.sort_by(|a, b| {
                for (idx, asc) in &keys {
                    let ord = OrdVal(a[*idx].clone()).cmp(&OrdVal(b[*idx].clone()));
                    let ord = if *asc { ord } else { ord.reverse() };
                    if ord != Ordering::Equal {
                        return ord;
                    }
                }
                Ordering::Equal
            });
            self.src = rows.into_iter();
            self.done = true;
        }
        self.src.next().map(Ok)
    }
}

/// Total-order wrapper matching the reference engine (numeric cross-type, else
/// `Value::total_cmp`).
#[derive(Clone, PartialEq, Eq)]
struct OrdVal(Value);
impl PartialOrd for OrdVal {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OrdVal {
    fn cmp(&self, other: &Self) -> Ordering {
        fn num(v: &Value) -> Option<f64> {
            match v {
                Value::Int(i) => Some(*i as f64),
                Value::BigInt(i) => Some(*i as f64),
                Value::Double(d) => Some(*d),
                _ => None,
            }
        }
        if let (Some(a), Some(b)) = (num(&self.0), num(&other.0)) {
            a.partial_cmp(&b).unwrap_or(Ordering::Equal)
        } else {
            self.0.total_cmp(&other.0)
        }
    }
}

/// Bindings for one table scanned under `alias`.
fn bindings(alias: &str, schema: &Schema) -> Sch {
    schema
        .columns
        .iter()
        .map(|c| (alias.to_string(), c.name.clone()))
        .collect()
}

/// Try to run `q` over a single table's `(schema, rows)` with the streaming
/// executor. Returns `None` if the query is outside the streaming subset (the
/// caller then uses the materializing path).
pub fn try_stream(
    q: &Select,
    table_alias: &str,
    schema: &Schema,
    rows: Vec<Row>,
) -> Option<Result<ResultSet, ExecError>> {
    let from = q.from.as_ref()?;
    if !from.joins.is_empty() {
        return None;
    }
    finish(q, &bindings(table_alias, schema), rows)
}

/// Try to run a **join** query with the streaming executor, then the shared
/// projection/aggregation/order/limit tail. When every join is inner and the ON
/// clauses form a clean equijoin graph, a **cost-based greedy reordering**
/// (`join_plan`) picks the left-deep order that minimizes estimated intermediate
/// cardinality; otherwise the tables fold in FROM order. Returns `None` — deferring
/// to the materializing oracle — whenever it cannot prove equivalence (a
/// non-equijoin ON, an ON that straddles both sides ambiguously, or a subquery).
pub fn try_stream_join(
    q: &Select,
    tables: &[(String, Schema, Vec<Row>)],
) -> Option<Result<ResultSet, ExecError>> {
    let from = q.from.as_ref()?;
    if from.joins.is_empty() {
        return None;
    }
    if from.joins.len() + 1 != tables.len() {
        return None;
    }

    let (sch, rows) = match join_plan(from, tables) {
        Some(steps) => fold_reordered(tables, &steps)?,
        None => fold_from_order(from, tables)?,
    };
    finish(q, &sch, rows)
}

/// The cost-based table order the planner would choose (as FROM-table indices), or
/// `None` if the query folds in FROM order. For tests / a mini-EXPLAIN.
pub fn planned_join_order(
    from: &keel_sql::FromClause,
    tables: &[(String, Schema, Vec<Row>)],
) -> Option<Vec<usize>> {
    join_plan(from, tables).map(|steps| steps.iter().map(|s| s.table).collect())
}

/// The FROM-order left-deep fold (the fallback when the plan can't be reordered —
/// e.g. an outer join or an ambiguous ON).
fn fold_from_order(
    from: &keel_sql::FromClause,
    tables: &[(String, Schema, Vec<Row>)],
) -> Option<(Sch, Vec<Row>)> {
    let (mut sch, mut rows) = {
        let (a, s, r) = &tables[0];
        (bindings(a, s), r.clone())
    };
    for (i, (kind, _tref, on)) in from.joins.iter().enumerate() {
        let (ralias, rschema, rrows) = &tables[i + 1];
        let rsch = bindings(ralias, rschema);
        let (lkey, rkey) = extract_equijoin(on, &sch, &rsch)?;
        let (nsch, nrows) = hash_join(*kind, &sch, rows, &lkey, &rsch, rrows, &rkey)?;
        sch = nsch;
        rows = nrows;
    }
    Some((sch, rows))
}

/// One step of a reordered plan: the table to fold in, the equijoin key pair
/// connecting it to the already-joined set (`None` for the starting table), and any
/// extra equijoin predicates to apply as a residual filter (a table connected by
/// more than one edge).
struct PlanStep {
    table: usize,
    key: Option<(Expr, Expr)>,
    residuals: Vec<Expr>,
}

/// An equijoin edge between two FROM tables, `akey`(of table `a`) `=` `bkey`(of `b`).
struct Edge {
    a: usize,
    akey: Expr,
    b: usize,
    bkey: Expr,
}

/// The unique table index whose schema binds every column of `e`; `None` if zero or
/// more than one table qualifies (an unqualified or straddling reference).
fn sole_table(e: &Expr, schs: &[Sch]) -> Option<usize> {
    let mut found = None;
    for (i, sch) in schs.iter().enumerate() {
        if binds_in(e, sch) {
            if found.is_some() {
                return None;
            }
            found = Some(i);
        }
    }
    found
}

/// Distinct non-null values of `key` over `rows` (for equijoin selectivity).
fn ndv(key: &Expr, sch: &Sch, rows: &[Row]) -> usize {
    let mut seen = std::collections::BTreeSet::new();
    for r in rows {
        if let Ok(v) = eval_public(key, sch, r) {
            if !v.is_null() {
                seen.insert(OrdVal(v));
            }
        }
    }
    seen.len().max(1)
}

/// Estimated selectivity of an equijoin edge: `1 / max(ndv_a, ndv_b)` — the textbook
/// estimate, computed exactly over the (in-memory) rows.
fn edge_selectivity(e: &Edge, tables: &[(String, Schema, Vec<Row>)], schs: &[Sch]) -> f64 {
    let na = ndv(&e.akey, &schs[e.a], &tables[e.a].2);
    let nb = ndv(&e.bkey, &schs[e.b], &tables[e.b].2);
    1.0 / na.max(nb) as f64
}

/// Compute a cost-based greedy join order. Returns `None` (fall back to FROM order)
/// unless every join is inner and every ON is a clean two-table equijoin, and the
/// graph is connected (no cross product).
fn join_plan(
    from: &keel_sql::FromClause,
    tables: &[(String, Schema, Vec<Row>)],
) -> Option<Vec<PlanStep>> {
    if from
        .joins
        .iter()
        .any(|(k, _, _)| *k != keel_sql::JoinKind::Inner)
    {
        return None;
    }
    let schs: Vec<Sch> = tables.iter().map(|(a, s, _)| bindings(a, s)).collect();
    let sizes: Vec<usize> = tables.iter().map(|(_, _, r)| r.len()).collect();

    let mut edges: Vec<Edge> = Vec::new();
    for (_, _, on) in &from.joins {
        let keel_sql::Expr::Binary {
            op: keel_sql::BinOp::Eq,
            left,
            right,
        } = on
        else {
            return None;
        };
        let lt = sole_table(left, &schs)?;
        let rt = sole_table(right, &schs)?;
        if lt == rt {
            return None;
        }
        edges.push(Edge {
            a: lt,
            akey: (**left).clone(),
            b: rt,
            bkey: (**right).clone(),
        });
    }

    let n = tables.len();
    if n > 12 {
        return None;
    }
    let order = dp_order(n, &sizes, &edges, tables, &schs)?;
    build_steps(&order, &edges)
}

/// True if `t` has an equijoin edge to any table in `mask`.
fn connects(t: usize, mask: u32, edges: &[Edge]) -> bool {
    edges
        .iter()
        .any(|e| (e.a == t && (mask >> e.b) & 1 == 1) || (e.b == t && (mask >> e.a) & 1 == 1))
}

/// An edge joining `t` to some table in `mask` (the caller has checked one exists).
fn connecting_edge(t: usize, mask: u32, edges: &[Edge]) -> &Edge {
    edges
        .iter()
        .find(|e| (e.a == t && (mask >> e.b) & 1 == 1) || (e.b == t && (mask >> e.a) & 1 == 1))
        .expect("connecting_edge requires a connected table")
}

/// **Selinger left-deep dynamic program.** `best[S]` is the minimum-cost left-deep
/// join order for the table set `S` (bitmask) and its estimated cardinality; cost
/// is the classic sum of intermediate cardinalities. Returns the optimal full
/// ordering, or `None` if the join graph is disconnected (a cross product).
fn dp_order(
    n: usize,
    sizes: &[usize],
    edges: &[Edge],
    tables: &[(String, Schema, Vec<Row>)],
    schs: &[Sch],
) -> Option<Vec<usize>> {
    let full = (1u32 << n) - 1;
    let mut best: Vec<Option<(f64, f64, Vec<usize>)>> = vec![None; 1 << n];
    for (i, &sz) in sizes.iter().enumerate().take(n) {
        best[1 << i] = Some((0.0, sz as f64, vec![i]));
    }
    for mask in 1u32..=full {
        if mask.count_ones() < 2 {
            continue;
        }
        #[allow(clippy::needless_range_loop)]
        for t in 0..n {
            if (mask >> t) & 1 == 0 {
                continue;
            }
            let prev = mask & !(1 << t);
            let Some((pcost, pcard, porder)) = best[prev as usize].clone() else {
                continue;
            };
            if !connects(t, prev, edges) {
                continue;
            }
            let sel = edge_selectivity(connecting_edge(t, prev, edges), tables, schs);
            let card = pcard * sizes[t] as f64 * sel;
            let cost = pcost + card;
            let improved = match &best[mask as usize] {
                Some((c, _, _)) => cost < *c,
                None => true,
            };
            if improved {
                let mut order = porder;
                order.push(t);
                best[mask as usize] = Some((cost, card, order));
            }
        }
    }
    best[full as usize].take().map(|(_, _, order)| order)
}

/// Turn a chosen table order into fold steps: each table after the first is keyed
/// by an edge to the already-joined prefix, with any further edges as residuals.
fn build_steps(order: &[usize], edges: &[Edge]) -> Option<Vec<PlanStep>> {
    let mut joined_mask: u32 = 1 << order[0];
    let mut steps = vec![PlanStep {
        table: order[0],
        key: None,
        residuals: Vec::new(),
    }];
    for &t in &order[1..] {
        let conn: Vec<&Edge> = edges
            .iter()
            .filter(|e| {
                (e.a == t && (joined_mask >> e.b) & 1 == 1)
                    || (e.b == t && (joined_mask >> e.a) & 1 == 1)
            })
            .collect();
        let primary = conn.first()?;
        let (jkey, tkey) = if primary.a == t {
            (primary.bkey.clone(), primary.akey.clone())
        } else {
            (primary.akey.clone(), primary.bkey.clone())
        };
        let residuals: Vec<Expr> = conn[1..]
            .iter()
            .map(|e| Expr::bin(keel_sql::BinOp::Eq, e.akey.clone(), e.bkey.clone()))
            .collect();
        steps.push(PlanStep {
            table: t,
            key: Some((jkey, tkey)),
            residuals,
        });
        joined_mask |= 1 << t;
    }
    Some(steps)
}

/// Fold the tables in the reordered plan's order (all inner joins), applying any
/// residual equijoin predicates after each hash join.
fn fold_reordered(
    tables: &[(String, Schema, Vec<Row>)],
    steps: &[PlanStep],
) -> Option<(Sch, Vec<Row>)> {
    let (mut sch, mut rows) = {
        let (a, s, r) = &tables[steps[0].table];
        (bindings(a, s), r.clone())
    };
    for step in &steps[1..] {
        let (talias, tschema, trows) = &tables[step.table];
        let tsch = bindings(talias, tschema);
        let (jkey, tkey) = step.key.as_ref().unwrap();
        let (nsch, mut nrows) = hash_join(
            keel_sql::JoinKind::Inner,
            &sch,
            rows,
            jkey,
            &tsch,
            trows,
            tkey,
        )?;
        for res in &step.residuals {
            let mut kept = Vec::with_capacity(nrows.len());
            for row in nrows {
                if let Value::Bool(true) = eval_public(res, &nsch, &row).ok()? {
                    kept.push(row);
                }
            }
            nrows = kept;
        }
        sch = nsch;
        rows = nrows;
    }
    Some((sch, rows))
}

/// The shared tail: expand SELECT items, apply WHERE, then either projection or
/// (for a grouped/aggregated query) hash aggregation, then Distinct/Sort/Limit over
/// `(sch, rows)`. `None` if any item or clause is ineligible for streaming.
fn finish(q: &Select, sch: &Sch, rows: Vec<Row>) -> Option<Result<ResultSet, ExecError>> {
    if let Some(f) = &q.filter {
        if !is_subquery_free(f) {
            return None;
        }
    }
    if let Some(h) = &q.having {
        if !is_subquery_free(h) {
            return None;
        }
    }
    if q.group_by.iter().any(|e| !is_subquery_free(e)) {
        return None;
    }

    let mut out_cols = Vec::new();
    let mut exprs = Vec::new();
    for item in &q.items {
        match item {
            SelectItem::Wildcard => {
                for (t, c) in sch {
                    out_cols.push(c.clone());
                    exprs.push(Expr::qcol(t, c));
                }
            }
            SelectItem::QualifiedWildcard(t) => {
                if !sch.iter().any(|(a, _)| a == t) {
                    return None;
                }
                for (a, c) in sch.iter().filter(|(a, _)| a == t) {
                    out_cols.push(c.clone());
                    exprs.push(Expr::qcol(a, c));
                }
            }
            SelectItem::Expr(e, alias) => {
                if !is_subquery_free(e) {
                    return None;
                }
                out_cols.push(alias.clone().unwrap_or_else(|| column_label(e)));
                exprs.push(e.clone());
            }
        }
    }

    let mut sort_keys = Vec::new();
    for k in &q.order_by {
        let idx = match &k.expr {
            Expr::Column { name, .. } => out_cols.iter().position(|c| c == name)?,
            Expr::Literal(Value::BigInt(n)) => {
                let i = (*n as usize).checked_sub(1)?;
                if i >= out_cols.len() {
                    return None;
                }
                i
            }
            _ => return None,
        };
        sort_keys.push((idx, k.asc));
    }

    let mut root: Box<dyn Op> = if needs_aggregation(q) {
        let agg_rows = match run_aggregate(q, sch, &rows, &exprs) {
            Ok(r) => r,
            Err(e) => return Some(Err(e)),
        };
        Box::new(Scan {
            it: agg_rows.into_iter(),
        })
    } else {
        let mut r: Box<dyn Op> = Box::new(Scan {
            it: rows.into_iter(),
        });
        if let Some(pred) = &q.filter {
            r = Box::new(Filter {
                child: r,
                pred: pred.clone(),
                schema: sch.clone(),
            });
        }
        Box::new(Project {
            child: r,
            exprs,
            schema: sch.clone(),
        })
    };
    if q.distinct {
        root = Box::new(Distinct {
            src: Vec::new().into_iter(),
            done: false,
            child: Some(root),
        });
    }
    if !sort_keys.is_empty() {
        root = Box::new(Sort {
            src: Vec::new().into_iter(),
            done: false,
            child: Some(root),
            keys: sort_keys,
        });
    }
    if let Some(n) = q.limit {
        root = Box::new(Limit {
            child: root,
            remaining: n.max(0) as usize,
        });
    }

    let mut out = Vec::new();
    while let Some(r) = root.next() {
        match r {
            Ok(row) => out.push(row),
            Err(e) => return Some(Err(e)),
        }
    }
    Some(Ok(ResultSet {
        columns: out_cols,
        rows: out,
    }))
}

/// Public: does this query produce grouped/aggregated output? (Telemetry, so the
/// caller can record that the streaming aggregate path — not the fallback — ran.)
pub fn is_aggregate(q: &Select) -> bool {
    needs_aggregation(q)
}

/// Whether `q` produces grouped/aggregated output.
fn needs_aggregation(q: &Select) -> bool {
    !q.group_by.is_empty()
        || q.having.is_some()
        || q.items
            .iter()
            .any(|it| matches!(it, SelectItem::Expr(e, _) if contains_agg(e)))
}

/// Run the aggregate path: WHERE-filter, group, reduce, apply HAVING, and project
/// each surviving group to a row (in `exprs` order).
fn run_aggregate(
    q: &Select,
    sch: &Sch,
    rows: &[Row],
    exprs: &[Expr],
) -> Result<Vec<Row>, ExecError> {
    let mut filtered: Vec<Row> = Vec::new();
    for r in rows {
        match &q.filter {
            Some(p) => {
                if matches!(eval_public(p, sch, r)?, Value::Bool(true)) {
                    filtered.push(r.clone());
                }
            }
            None => filtered.push(r.clone()),
        }
    }
    let groups = group_rows(&q.group_by, sch, &filtered)?;
    let mut out = Vec::new();
    for g in &groups {
        let rep: Row = g
            .first()
            .cloned()
            .unwrap_or_else(|| vec![Value::Null; sch.len()]);
        if let Some(h) = &q.having {
            let hv = eval_public(&substitute_aggs(h, sch, g)?, sch, &rep)?;
            if !matches!(hv, Value::Bool(true)) {
                continue;
            }
        }
        let mut orow = Vec::with_capacity(exprs.len());
        for e in exprs {
            orow.push(eval_public(&substitute_aggs(e, sch, g)?, sch, &rep)?);
        }
        out.push(orow);
    }
    Ok(out)
}

/// Group `rows` by the GROUP BY key values, preserving first-seen order. An empty
/// GROUP BY is a single group over all rows (so `COUNT(*)` on no rows is 0).
fn group_rows(group_by: &[Expr], sch: &Sch, rows: &[Row]) -> Result<Vec<Vec<Row>>, ExecError> {
    if group_by.is_empty() {
        return Ok(vec![rows.to_vec()]);
    }
    let mut groups: Vec<Vec<Row>> = Vec::new();
    let mut index: std::collections::BTreeMap<Vec<OrdVal>, usize> =
        std::collections::BTreeMap::new();
    for r in rows {
        let key: Vec<OrdVal> = group_by
            .iter()
            .map(|e| eval_public(e, sch, r).map(OrdVal))
            .collect::<Result<_, _>>()?;
        match index.get(&key) {
            Some(&i) => groups[i].push(r.clone()),
            None => {
                index.insert(key, groups.len());
                groups.push(vec![r.clone()]);
            }
        }
    }
    Ok(groups)
}

/// Replace every aggregate node in `e` with a literal of its value over `group`;
/// the rest of the tree is left for `eval_public`.
fn substitute_aggs(e: &Expr, sch: &Sch, group: &[Row]) -> Result<Expr, ExecError> {
    Ok(match e {
        Expr::Aggregate {
            func,
            arg,
            distinct,
        } => Expr::Literal(reduce_aggregate(
            *func,
            arg.as_deref(),
            *distinct,
            sch,
            group,
        )?),
        Expr::Binary { op, left, right } => Expr::Binary {
            op: *op,
            left: Box::new(substitute_aggs(left, sch, group)?),
            right: Box::new(substitute_aggs(right, sch, group)?),
        },
        Expr::Unary { op, expr } => Expr::Unary {
            op: *op,
            expr: Box::new(substitute_aggs(expr, sch, group)?),
        },
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: Box::new(substitute_aggs(expr, sch, group)?),
            negated: *negated,
        },
        Expr::InList {
            expr,
            list,
            negated,
        } => Expr::InList {
            expr: Box::new(substitute_aggs(expr, sch, group)?),
            list: list
                .iter()
                .map(|x| substitute_aggs(x, sch, group))
                .collect::<Result<_, _>>()?,
            negated: *negated,
        },
        Expr::Case {
            operand,
            whens,
            els,
        } => Expr::Case {
            operand: operand
                .as_ref()
                .map(|o| substitute_aggs(o, sch, group).map(Box::new))
                .transpose()?,
            whens: whens
                .iter()
                .map(|(c, v)| {
                    Ok((
                        substitute_aggs(c, sch, group)?,
                        substitute_aggs(v, sch, group)?,
                    ))
                })
                .collect::<Result<_, _>>()?,
            els: els
                .as_ref()
                .map(|x| substitute_aggs(x, sch, group).map(Box::new))
                .transpose()?,
        },
        other => other.clone(),
    })
}

/// Reduce one aggregate over a group's rows (NULLs skipped, per SQL).
fn reduce_aggregate(
    func: AggFunc,
    arg: Option<&Expr>,
    distinct: bool,
    sch: &Sch,
    group: &[Row],
) -> Result<Value, ExecError> {
    if func == AggFunc::Count && arg.is_none() {
        return Ok(Value::BigInt(group.len() as i64));
    }
    let arg = arg.ok_or_else(|| ExecError("aggregate requires an argument".into()))?;
    let mut vals: Vec<Value> = Vec::new();
    for r in group {
        let v = eval_public(arg, sch, r)?;
        if !v.is_null() {
            vals.push(v);
        }
    }
    if distinct {
        let mut seen = std::collections::BTreeSet::new();
        vals.retain(|v| seen.insert(OrdVal(v.clone())));
    }
    Ok(match func {
        AggFunc::Count => Value::BigInt(vals.len() as i64),
        AggFunc::Min => vals
            .into_iter()
            .min_by(|a, b| OrdVal(a.clone()).cmp(&OrdVal(b.clone())))
            .unwrap_or(Value::Null),
        AggFunc::Max => vals
            .into_iter()
            .max_by(|a, b| OrdVal(a.clone()).cmp(&OrdVal(b.clone())))
            .unwrap_or(Value::Null),
        AggFunc::Sum => {
            if vals.is_empty() {
                Value::Null
            } else {
                sum_values(&vals)
            }
        }
        AggFunc::Avg => {
            if vals.is_empty() {
                Value::Null
            } else {
                Value::Double(f64_sum(&vals) / vals.len() as f64)
            }
        }
    })
}

fn f64_of(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::BigInt(i) => Some(*i as f64),
        Value::Double(d) => Some(*d),
        _ => None,
    }
}
fn f64_sum(vals: &[Value]) -> f64 {
    vals.iter().filter_map(f64_of).sum()
}
/// SUM: integer inputs stay a wrapping i64 sum; any float promotes to f64 (matching
/// the reference engine's rule).
fn sum_values(vals: &[Value]) -> Value {
    if vals.iter().any(|v| matches!(v, Value::Double(_))) {
        Value::Double(f64_sum(vals))
    } else {
        let s = vals.iter().fold(0i64, |a, v| match v {
            Value::Int(i) => a.wrapping_add(*i as i64),
            Value::BigInt(i) => a.wrapping_add(*i),
            _ => a,
        });
        Value::BigInt(s)
    }
}

/// Does every column referenced by `e` resolve (unambiguously) in `sch`?
fn binds_in(e: &Expr, sch: &Sch) -> bool {
    match e {
        Expr::Column { table, name } => {
            sch.iter()
                .filter(|(a, c)| c == name && table.as_ref().map(|t| t == a).unwrap_or(true))
                .count()
                == 1
        }
        Expr::Literal(_) => true,
        Expr::Binary { left, right, .. } => binds_in(left, sch) && binds_in(right, sch),
        Expr::Unary { expr, .. } | Expr::IsNull { expr, .. } => binds_in(expr, sch),
        _ => false,
    }
}

/// If `on` is a pure equijoin `L = R` with one side resolving only in the left
/// schema and the other only in the right, return `(left_key, right_key)`.
fn extract_equijoin(on: &Expr, lsch: &Sch, rsch: &Sch) -> Option<(Expr, Expr)> {
    let Expr::Binary {
        op: keel_sql::BinOp::Eq,
        left,
        right,
    } = on
    else {
        return None;
    };
    if binds_in(left, lsch)
        && !binds_in(left, rsch)
        && binds_in(right, rsch)
        && !binds_in(right, lsch)
    {
        Some(((**left).clone(), (**right).clone()))
    } else if binds_in(right, lsch)
        && !binds_in(right, rsch)
        && binds_in(left, rsch)
        && !binds_in(left, lsch)
    {
        Some(((**right).clone(), (**left).clone()))
    } else {
        None
    }
}

/// Hash-join the left stream against the right table on `lkey = rkey`. Builds the
/// hash table on the right side; NULL keys never match (SQL `NULL = NULL` is not
/// TRUE). For a LEFT join, unmatched left rows emit NULLs for the right columns.
fn hash_join(
    kind: keel_sql::JoinKind,
    lsch: &Sch,
    lrows: Vec<Row>,
    lkey: &Expr,
    rsch: &Sch,
    rrows: &[Row],
    rkey: &Expr,
) -> Option<(Sch, Vec<Row>)> {
    use std::collections::BTreeMap;
    let mut build: BTreeMap<OrdVal, Vec<usize>> = BTreeMap::new();
    for (j, rrow) in rrows.iter().enumerate() {
        let k = eval_public(rkey, rsch, rrow).ok()?;
        if !k.is_null() {
            build.entry(OrdVal(k)).or_default().push(j);
        }
    }
    let mut combined = lsch.clone();
    combined.extend(rsch.iter().cloned());
    let rwidth = rsch.len();
    let mut out = Vec::new();
    for lrow in lrows {
        let k = eval_public(lkey, lsch, &lrow).ok()?;
        let matches = if k.is_null() {
            None
        } else {
            build.get(&OrdVal(k))
        };
        match matches {
            Some(js) if !js.is_empty() => {
                for &j in js {
                    let mut row = lrow.clone();
                    row.extend(rrows[j].iter().cloned());
                    out.push(row);
                }
            }
            _ => {
                if kind == keel_sql::JoinKind::Left {
                    let mut row = lrow.clone();
                    row.extend(std::iter::repeat_n(Value::Null, rwidth));
                    out.push(row);
                }
            }
        }
    }
    Some((combined, out))
}

fn contains_agg(e: &Expr) -> bool {
    match e {
        Expr::Aggregate { .. } => true,
        Expr::Binary { left, right, .. } => contains_agg(left) || contains_agg(right),
        Expr::Unary { expr, .. } | Expr::IsNull { expr, .. } => contains_agg(expr),
        Expr::InList { expr, list, .. } => contains_agg(expr) || list.iter().any(contains_agg),
        Expr::Case {
            operand,
            whens,
            els,
        } => {
            operand.as_deref().map(contains_agg).unwrap_or(false)
                || whens
                    .iter()
                    .any(|(a, b)| contains_agg(a) || contains_agg(b))
                || els.as_deref().map(contains_agg).unwrap_or(false)
        }
        _ => false,
    }
}
