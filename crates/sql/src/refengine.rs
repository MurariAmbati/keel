use std::cmp::Ordering;
use std::collections::BTreeMap;

use keel_types::{ColumnDef, ColumnType, Schema, Value};

use crate::ast::*;

pub type Row = Vec<Value>;

#[derive(Clone, Debug, PartialEq)]
pub struct ExecError(pub String);
impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exec error: {}", self.0)
    }
}
impl std::error::Error for ExecError {}
fn err<T>(m: impl Into<String>) -> Result<T, ExecError> {
    Err(ExecError(m.into()))
}
type R<T> = Result<T, ExecError>;

#[derive(Clone, Debug, PartialEq)]
pub struct ResultSet {
    pub columns: Vec<String>,
    pub rows: Vec<Row>,
}

#[derive(Clone, Debug)]
struct Table {
    schema: Schema,
    rows: Vec<Row>,
}

#[derive(Clone, Debug, Default)]
pub struct MemDb {
    tables: BTreeMap<String, Table>,
}

impl MemDb {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn table_names(&self) -> Vec<String> {
        self.tables.keys().cloned().collect()
    }

    pub fn install_table(&mut self, name: &str, schema: Schema, rows: Vec<Row>) {
        self.tables.insert(name.to_string(), Table { schema, rows });
    }

    pub fn schema_of(&self, name: &str) -> Option<&Schema> {
        self.tables.get(name).map(|t| &t.schema)
    }

    pub fn rows_of(&self, name: &str) -> Option<&[Row]> {
        self.tables.get(name).map(|t| t.rows.as_slice())
    }

    pub fn execute(&mut self, stmt: &Stmt) -> R<Option<ResultSet>> {
        match stmt {
            Stmt::CreateTable(ct) => {
                self.create_table(ct)?;
                Ok(None)
            }
            Stmt::CreateIndex(_) => Ok(None),
            Stmt::Insert(ins) => {
                self.insert(ins)?;
                Ok(None)
            }
            Stmt::Delete(d) => {
                self.delete(d)?;
                Ok(None)
            }
            Stmt::Update(u) => {
                self.update(u)?;
                Ok(None)
            }
            Stmt::Select(q) => Ok(Some(self.run_select(q)?)),
            Stmt::DropTable(name) => {
                if self.tables.remove(name).is_none() {
                    return err(format!("no such table '{name}'"));
                }
                Ok(None)
            }
            Stmt::DropIndex(_) => Ok(None),
            Stmt::Begin | Stmt::Commit | Stmt::Rollback => Ok(None),
        }
    }

    fn delete(&mut self, del: &Delete) -> R<()> {
        let table = self
            .tables
            .get(&del.table)
            .ok_or_else(|| ExecError(format!("no such table '{}'", del.table)))?;
        let bindings = single_table_bindings(&del.table, &table.schema);
        let rows = table.rows.clone();
        let mut keep = Vec::with_capacity(rows.len());
        for row in rows {
            let matched = match &del.filter {
                None => true,
                Some(pred) => {
                    let ctx = EvalCtx {
                        bindings: &bindings,
                        row: &row,
                        db: self,
                    };
                    truthy(eval(pred, &ctx)?)
                }
            };
            if !matched {
                keep.push(row);
            }
        }
        self.tables.get_mut(&del.table).unwrap().rows = keep;
        Ok(())
    }

    fn update(&mut self, upd: &Update) -> R<()> {
        let table = self
            .tables
            .get(&upd.table)
            .ok_or_else(|| ExecError(format!("no such table '{}'", upd.table)))?;
        let schema = table.schema.clone();
        let bindings = single_table_bindings(&upd.table, &schema);
        let targets: Vec<(usize, &Expr)> = upd
            .assignments
            .iter()
            .map(|(col, e)| {
                schema
                    .column_index(col)
                    .map(|i| (i, e))
                    .ok_or_else(|| ExecError(format!("no column '{col}'")))
            })
            .collect::<R<Vec<_>>>()?;
        let mut rows = table.rows.clone();
        for row in rows.iter_mut() {
            let matched = match &upd.filter {
                None => true,
                Some(pred) => {
                    let ctx = EvalCtx {
                        bindings: &bindings,
                        row,
                        db: self,
                    };
                    truthy(eval(pred, &ctx)?)
                }
            };
            if !matched {
                continue;
            }
            let mut newvals = Vec::with_capacity(targets.len());
            for (idx, e) in &targets {
                let ctx = EvalCtx {
                    bindings: &bindings,
                    row,
                    db: self,
                };
                let v = eval(e, &ctx)?;
                newvals.push((*idx, coerce(v, schema.columns[*idx].ty)?));
            }
            for (idx, v) in newvals {
                if schema.columns[idx].not_null && v.is_null() {
                    return err(format!(
                        "NULL in NOT NULL column '{}'",
                        schema.columns[idx].name
                    ));
                }
                row[idx] = v;
            }
        }
        self.tables.get_mut(&upd.table).unwrap().rows = rows;
        Ok(())
    }

    pub fn query(&self, q: &Select) -> R<ResultSet> {
        self.run_select(q)
    }

    fn create_table(&mut self, ct: &CreateTable) -> R<()> {
        if self.tables.contains_key(&ct.name) {
            return err(format!("table '{}' already exists", ct.name));
        }
        let columns = ct
            .columns
            .iter()
            .map(|c| ColumnDef::new(c.name.clone(), c.ty, c.not_null))
            .collect();
        self.tables.insert(
            ct.name.clone(),
            Table {
                schema: Schema::new(columns),
                rows: Vec::new(),
            },
        );
        Ok(())
    }

    fn insert(&mut self, ins: &Insert) -> R<()> {
        let schema = self
            .tables
            .get(&ins.table)
            .ok_or_else(|| ExecError(format!("no such table '{}'", ins.table)))?
            .schema
            .clone();
        let order: Vec<usize> = match &ins.columns {
            None => (0..schema.len()).collect(),
            Some(cols) => cols
                .iter()
                .map(|c| {
                    schema
                        .column_index(c)
                        .ok_or_else(|| ExecError(format!("no column '{c}'")))
                })
                .collect::<R<Vec<_>>>()?,
        };
        let mut new_rows = Vec::new();
        for exprs in &ins.rows {
            if exprs.len() != order.len() {
                return err("INSERT value count does not match column count");
            }
            let mut row = vec![Value::Null; schema.len()];
            for (slot, e) in order.iter().zip(exprs) {
                let v = eval_const(e)?;
                row[*slot] = coerce(v, schema.columns[*slot].ty)?;
            }
            for (i, col) in schema.columns.iter().enumerate() {
                if col.not_null && row[i].is_null() {
                    return err(format!("NULL in NOT NULL column '{}'", col.name));
                }
            }
            new_rows.push(row);
        }
        self.tables
            .get_mut(&ins.table)
            .unwrap()
            .rows
            .extend(new_rows);
        Ok(())
    }

    fn run_select(&self, q: &Select) -> R<ResultSet> {
        let (bindings, frames0) = self.build_from(q)?;

        let frames: Vec<Row> = match &q.filter {
            None => frames0,
            Some(pred) => {
                let mut kept = Vec::new();
                for row in frames0 {
                    let ctx = EvalCtx {
                        bindings: &bindings,
                        row: &row,
                        db: self,
                    };
                    if truthy(eval(pred, &ctx)?) {
                        kept.push(row);
                    }
                }
                kept
            }
        };

        let has_agg = q
            .items
            .iter()
            .any(|it| matches!(it, SelectItem::Expr(e, _) if contains_agg(e)))
            || q.having.as_ref().map(contains_agg).unwrap_or(false)
            || !q.group_by.is_empty();

        let mut out_cols = Vec::new();
        let mut rk: Vec<(Row, Vec<Value>)> = Vec::new();

        if has_agg {
            let groups = self.make_groups(q, &bindings, &frames)?;
            let exprs = self.expand_items(q, &bindings, &mut out_cols)?;
            for (key, grows) in &groups {
                if let Some(h) = &q.having {
                    if !truthy(eval_grouped(h, &bindings, grows, &q.group_by, key, self)?) {
                        continue;
                    }
                }
                let out_row: Row = exprs
                    .iter()
                    .map(|e| eval_grouped(e, &bindings, grows, &q.group_by, key, self))
                    .collect::<R<_>>()?;
                let keys = q
                    .order_by
                    .iter()
                    .map(|ok| {
                        resolve_order_key(&ok.expr, &out_cols, &out_row, |e| {
                            eval_grouped(e, &bindings, grows, &q.group_by, key, self)
                        })
                    })
                    .collect::<R<Vec<_>>>()?;
                rk.push((out_row, keys));
            }
        } else {
            let exprs = self.expand_items(q, &bindings, &mut out_cols)?;
            for row in &frames {
                let ctx = EvalCtx {
                    bindings: &bindings,
                    row,
                    db: self,
                };
                let out_row: Row = exprs.iter().map(|e| eval(e, &ctx)).collect::<R<_>>()?;
                let keys = q
                    .order_by
                    .iter()
                    .map(|ok| resolve_order_key(&ok.expr, &out_cols, &out_row, |e| eval(e, &ctx)))
                    .collect::<R<Vec<_>>>()?;
                rk.push((out_row, keys));
            }
        }

        if q.distinct {
            let mut seen: BTreeMap<Vec<OrdVal>, ()> = BTreeMap::new();
            rk.retain(|(r, _)| {
                let key: Vec<OrdVal> = r.iter().cloned().map(OrdVal).collect();
                seen.insert(key, ()).is_none()
            });
        }

        if !q.order_by.is_empty() {
            let ascs: Vec<bool> = q.order_by.iter().map(|k| k.asc).collect();
            rk.sort_by(|a, b| {
                for (i, asc) in ascs.iter().enumerate() {
                    let ord = OrdVal(a.1[i].clone()).cmp(&OrdVal(b.1[i].clone()));
                    let ord = if *asc { ord } else { ord.reverse() };
                    if ord != Ordering::Equal {
                        return ord;
                    }
                }
                Ordering::Equal
            });
        }

        let mut out_rows: Vec<Row> = rk.into_iter().map(|(r, _)| r).collect();
        if let Some(n) = q.limit {
            out_rows.truncate(n.max(0) as usize);
        }
        Ok(ResultSet {
            columns: out_cols,
            rows: out_rows,
        })
    }

    fn make_groups(
        &self,
        q: &Select,
        bindings: &[Binding],
        frames: &[Row],
    ) -> R<Vec<(Vec<Value>, Vec<Row>)>> {
        if q.group_by.is_empty() {
            return Ok(vec![(Vec::new(), frames.to_vec())]);
        }
        let mut groups: Vec<(Vec<Value>, Vec<Row>)> = Vec::new();
        let mut index: BTreeMap<Vec<OrdVal>, usize> = BTreeMap::new();
        for row in frames {
            let ctx = EvalCtx {
                bindings,
                row,
                db: self,
            };
            let key: Vec<Value> = q.group_by.iter().map(|e| eval(e, &ctx)).collect::<R<_>>()?;
            let okey: Vec<OrdVal> = key.iter().cloned().map(OrdVal).collect();
            match index.get(&okey) {
                Some(&gi) => groups[gi].1.push(row.clone()),
                None => {
                    index.insert(okey, groups.len());
                    groups.push((key, vec![row.clone()]));
                }
            }
        }
        Ok(groups)
    }

    fn build_from(&self, q: &Select) -> R<(Vec<Binding>, Vec<Row>)> {
        let Some(from) = &q.from else {
            return Ok((Vec::new(), vec![Vec::new()]));
        };
        let (mut bindings, mut rows) = self.table_frames(&from.first)?;
        for (kind, tref, on) in &from.joins {
            let (rb, rrows) = self.table_frames(tref)?;
            let mut combined_bindings = bindings.clone();
            combined_bindings.extend(rb.clone());
            let mut out = Vec::new();
            for lrow in &rows {
                let mut matched = false;
                for rrow in &rrows {
                    let mut row = lrow.clone();
                    row.extend(rrow.clone());
                    let ctx = EvalCtx {
                        bindings: &combined_bindings,
                        row: &row,
                        db: self,
                    };
                    if truthy(eval(on, &ctx)?) {
                        out.push(row);
                        matched = true;
                    }
                }
                if !matched && *kind == JoinKind::Left {
                    let mut row = lrow.clone();
                    row.extend(std::iter::repeat_n(Value::Null, rb.len()));
                    out.push(row);
                }
            }
            bindings = combined_bindings;
            rows = out;
        }
        Ok((bindings, rows))
    }

    fn table_frames(&self, tref: &TableRef) -> R<(Vec<Binding>, Vec<Row>)> {
        let table = self
            .tables
            .get(&tref.table)
            .ok_or_else(|| ExecError(format!("no such table '{}'", tref.table)))?;
        let alias = tref.alias.clone().unwrap_or_else(|| tref.table.clone());
        let bindings = table
            .schema
            .columns
            .iter()
            .map(|c| Binding {
                table: alias.clone(),
                col: c.name.clone(),
                ty: c.ty,
            })
            .collect();
        Ok((bindings, table.rows.clone()))
    }

    fn expand_items(
        &self,
        q: &Select,
        bindings: &[Binding],
        out_cols: &mut Vec<String>,
    ) -> R<Vec<Expr>> {
        let mut exprs = Vec::new();
        for item in &q.items {
            match item {
                SelectItem::Wildcard => {
                    for b in bindings {
                        exprs.push(Expr::qcol(&b.table, &b.col));
                        out_cols.push(b.col.clone());
                    }
                }
                SelectItem::QualifiedWildcard(t) => {
                    for b in bindings.iter().filter(|b| &b.table == t) {
                        exprs.push(Expr::qcol(&b.table, &b.col));
                        out_cols.push(b.col.clone());
                    }
                }
                SelectItem::Expr(e, alias) => {
                    out_cols.push(alias.clone().unwrap_or_else(|| expr_label(e)));
                    exprs.push(e.clone());
                }
            }
        }
        Ok(exprs)
    }
}

fn resolve_order_key(
    expr: &Expr,
    out_cols: &[String],
    out_row: &[Value],
    eval_source: impl Fn(&Expr) -> R<Value>,
) -> R<Value> {
    match expr {
        Expr::Column { name, .. } => {
            if let Some(i) = out_cols.iter().position(|c| c == name) {
                return Ok(out_row[i].clone());
            }
            eval_source(expr)
        }
        Expr::Literal(Value::BigInt(n)) => {
            let idx = (*n as usize).saturating_sub(1);
            out_row
                .get(idx)
                .cloned()
                .ok_or_else(|| ExecError(format!("ORDER BY position {n} out of range")))
        }
        _ => eval_source(expr),
    }
}

#[derive(Clone, Debug)]
struct Binding {
    table: String,
    col: String,
    #[allow(dead_code)]
    ty: ColumnType,
}

fn single_table_bindings(alias: &str, schema: &Schema) -> Vec<Binding> {
    schema
        .columns
        .iter()
        .map(|c| Binding {
            table: alias.to_string(),
            col: c.name.clone(),
            ty: c.ty,
        })
        .collect()
}

struct EvalCtx<'a> {
    bindings: &'a [Binding],
    row: &'a [Value],
    db: &'a MemDb,
}

impl EvalCtx<'_> {
    fn resolve(&self, table: &Option<String>, name: &str) -> R<Value> {
        let mut found = None;
        for (i, b) in self.bindings.iter().enumerate() {
            let table_ok = table.as_ref().map(|t| t == &b.table).unwrap_or(true);
            if table_ok && b.col == name {
                if found.is_some() {
                    return err(format!("ambiguous column '{name}'"));
                }
                found = Some(i);
            }
        }
        match found {
            Some(i) => Ok(self.row[i].clone()),
            None => err(format!("unknown column '{name}'")),
        }
    }
}

fn eval_const(e: &Expr) -> R<Value> {
    let db = MemDb::new();
    let ctx = EvalCtx {
        bindings: &[],
        row: &[],
        db: &db,
    };
    eval(e, &ctx)
}

pub fn eval_literal(e: &Expr) -> Result<Value, ExecError> {
    eval_const(e)
}

pub fn coerce_into(v: Value, ty: ColumnType) -> Result<Value, ExecError> {
    coerce(v, ty)
}

pub fn eval_public(
    expr: &Expr,
    cols: &[(String, String)],
    row: &[Value],
) -> Result<Value, ExecError> {
    let bindings: Vec<Binding> = cols
        .iter()
        .map(|(t, c)| Binding {
            table: t.clone(),
            col: c.clone(),
            ty: ColumnType::Int,
        })
        .collect();
    let db = MemDb::new();
    let ctx = EvalCtx {
        bindings: &bindings,
        row,
        db: &db,
    };
    eval(expr, &ctx)
}

pub fn column_label(e: &Expr) -> String {
    expr_label(e)
}

pub fn is_subquery_free(e: &Expr) -> bool {
    match e {
        Expr::ScalarSubquery(_) | Expr::InSubquery { .. } => false,
        Expr::Literal(_) | Expr::Column { .. } => true,
        Expr::Binary { left, right, .. } => is_subquery_free(left) && is_subquery_free(right),
        Expr::Unary { expr, .. } | Expr::IsNull { expr, .. } => is_subquery_free(expr),
        Expr::InList { expr, list, .. } => {
            is_subquery_free(expr) && list.iter().all(is_subquery_free)
        }
        Expr::Case {
            operand,
            whens,
            els,
        } => {
            operand.as_deref().map(is_subquery_free).unwrap_or(true)
                && whens
                    .iter()
                    .all(|(a, b)| is_subquery_free(a) && is_subquery_free(b))
                && els.as_deref().map(is_subquery_free).unwrap_or(true)
        }
        Expr::Aggregate { arg, .. } => arg.as_deref().map(is_subquery_free).unwrap_or(true),
    }
}

fn eval(e: &Expr, ctx: &EvalCtx) -> R<Value> {
    match e {
        Expr::Literal(v) => Ok(v.clone()),
        Expr::Column { table, name } => ctx.resolve(table, name),
        Expr::Unary { op, expr } => {
            let v = eval(expr, ctx)?;
            match op {
                UnOp::Neg => arith_neg(v),
                UnOp::Not => Ok(not3(to_bool3(&v))),
            }
        }
        Expr::Binary { op, left, right } => match op {
            BinOp::And => {
                let l = to_bool3(&eval(left, ctx)?);
                if l == Some(false) {
                    return Ok(Value::Bool(false));
                }
                let r = to_bool3(&eval(right, ctx)?);
                Ok(and3(l, r))
            }
            BinOp::Or => {
                let l = to_bool3(&eval(left, ctx)?);
                if l == Some(true) {
                    return Ok(Value::Bool(true));
                }
                let r = to_bool3(&eval(right, ctx)?);
                Ok(or3(l, r))
            }
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => {
                let l = eval(left, ctx)?;
                let r = eval(right, ctx)?;
                arith(*op, l, r)
            }
            _ => {
                let l = eval(left, ctx)?;
                let r = eval(right, ctx)?;
                compare(*op, l, r)
            }
        },
        Expr::IsNull { expr, negated } => {
            let v = eval(expr, ctx)?;
            Ok(Value::Bool(v.is_null() != *negated))
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let v = eval(expr, ctx)?;
            let items = list.iter().map(|e| eval(e, ctx)).collect::<R<Vec<_>>>()?;
            Ok(in_result(&v, items.iter(), *negated))
        }
        Expr::InSubquery {
            expr,
            query,
            negated,
        } => {
            let v = eval(expr, ctx)?;
            let rs = ctx.db.run_select(query)?;
            let items: Vec<Value> = rs.rows.iter().map(|r| r[0].clone()).collect();
            Ok(in_result(&v, items.iter(), *negated))
        }
        Expr::ScalarSubquery(query) => {
            let rs = ctx.db.run_select(query)?;
            match rs.rows.len() {
                0 => Ok(Value::Null),
                1 => Ok(rs.rows[0][0].clone()),
                _ => err("scalar subquery returned more than one row"),
            }
        }
        Expr::Case {
            operand,
            whens,
            els,
        } => {
            let opval = operand.as_ref().map(|o| eval(o, ctx)).transpose()?;
            for (cond, val) in whens {
                let matched = match &opval {
                    Some(ov) => matches!(
                        compare(BinOp::Eq, ov.clone(), eval(cond, ctx)?)?,
                        Value::Bool(true)
                    ),
                    None => truthy(eval(cond, ctx)?),
                };
                if matched {
                    return eval(val, ctx);
                }
            }
            match els {
                Some(e) => eval(e, ctx),
                None => Ok(Value::Null),
            }
        }
        Expr::Aggregate { .. } => err("aggregate used outside an aggregate context"),
    }
}

fn eval_grouped(
    e: &Expr,
    bindings: &[Binding],
    grows: &[Row],
    group_by: &[Expr],
    key: &[Value],
    db: &MemDb,
) -> R<Value> {
    if let Some(pos) = group_by.iter().position(|g| g == e) {
        return Ok(key[pos].clone());
    }
    match e {
        Expr::Aggregate {
            func,
            arg,
            distinct,
        } => aggregate(*func, arg.as_deref(), *distinct, bindings, grows, db),
        Expr::Literal(v) => Ok(v.clone()),
        Expr::Column { .. } => match grows.first() {
            Some(row) => {
                let ctx = EvalCtx { bindings, row, db };
                eval(e, &ctx)
            }
            None => Ok(Value::Null),
        },
        Expr::Unary { op, expr } => {
            let v = eval_grouped(expr, bindings, grows, group_by, key, db)?;
            match op {
                UnOp::Neg => arith_neg(v),
                UnOp::Not => Ok(not3(to_bool3(&v))),
            }
        }
        Expr::Binary { op, left, right } => {
            let l = eval_grouped(left, bindings, grows, group_by, key, db)?;
            let r = eval_grouped(right, bindings, grows, group_by, key, db)?;
            match op {
                BinOp::And => Ok(and3(to_bool3(&l), to_bool3(&r))),
                BinOp::Or => Ok(or3(to_bool3(&l), to_bool3(&r))),
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => arith(*op, l, r),
                _ => compare(*op, l, r),
            }
        }
        Expr::IsNull { expr, negated } => {
            let v = eval_grouped(expr, bindings, grows, group_by, key, db)?;
            Ok(Value::Bool(v.is_null() != *negated))
        }
        Expr::Case {
            operand,
            whens,
            els,
        } => {
            let opval = operand
                .as_ref()
                .map(|o| eval_grouped(o, bindings, grows, group_by, key, db))
                .transpose()?;
            for (cond, val) in whens {
                let cv = eval_grouped(cond, bindings, grows, group_by, key, db)?;
                let matched = match &opval {
                    Some(ov) => matches!(compare(BinOp::Eq, ov.clone(), cv)?, Value::Bool(true)),
                    None => truthy(cv),
                };
                if matched {
                    return eval_grouped(val, bindings, grows, group_by, key, db);
                }
            }
            match els {
                Some(e) => eval_grouped(e, bindings, grows, group_by, key, db),
                None => Ok(Value::Null),
            }
        }
        _ => err("unsupported expression in aggregate context"),
    }
}

fn aggregate(
    func: AggFunc,
    arg: Option<&Expr>,
    distinct: bool,
    bindings: &[Binding],
    grows: &[Row],
    db: &MemDb,
) -> R<Value> {
    if func == AggFunc::Count && arg.is_none() {
        return Ok(Value::BigInt(grows.len() as i64));
    }
    let arg = arg.expect("non-count aggregate needs an argument");
    let mut vals: Vec<Value> = Vec::new();
    for row in grows {
        let ctx = EvalCtx { bindings, row, db };
        let v = eval(arg, &ctx)?;
        if !v.is_null() {
            vals.push(v);
        }
    }
    if distinct {
        vals = dedup(vals.into_iter().map(|v| vec![v]).collect())
            .into_iter()
            .map(|mut r| r.pop().unwrap())
            .collect();
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
                sum_values(&vals)?
            }
        }
        AggFunc::Avg => {
            if vals.is_empty() {
                Value::Null
            } else {
                let n = vals.len() as f64;
                let s = to_f64_sum(&vals)?;
                Value::Double(s / n)
            }
        }
    })
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
fn and3(l: Option<bool>, r: Option<bool>) -> Value {
    match (l, r) {
        (Some(false), _) | (_, Some(false)) => Value::Bool(false),
        (Some(true), Some(true)) => Value::Bool(true),
        _ => Value::Null,
    }
}
fn or3(l: Option<bool>, r: Option<bool>) -> Value {
    match (l, r) {
        (Some(true), _) | (_, Some(true)) => Value::Bool(true),
        (Some(false), Some(false)) => Value::Bool(false),
        _ => Value::Null,
    }
}
fn truthy(v: Value) -> bool {
    matches!(v, Value::Bool(true))
}

fn num(v: &Value) -> Option<(f64, bool)> {
    match v {
        Value::Int(i) => Some((*i as f64, false)),
        Value::BigInt(i) => Some((*i as f64, false)),
        Value::Double(d) => Some((*d, true)),
        _ => None,
    }
}

fn arith(op: BinOp, l: Value, r: Value) -> R<Value> {
    if l.is_null() || r.is_null() {
        return Ok(Value::Null);
    }
    let (lf, lfl) = num(&l).ok_or_else(|| ExecError("arithmetic on non-numeric".into()))?;
    let (rf, rfl) = num(&r).ok_or_else(|| ExecError("arithmetic on non-numeric".into()))?;
    let is_float = lfl || rfl || op == BinOp::Div;
    if is_float {
        let res = match op {
            BinOp::Add => lf + rf,
            BinOp::Sub => lf - rf,
            BinOp::Mul => lf * rf,
            BinOp::Div => {
                if rf == 0.0 {
                    return err("division by zero");
                }
                lf / rf
            }
            _ => unreachable!(),
        };
        Ok(Value::Double(res))
    } else {
        let li = lf as i64;
        let ri = rf as i64;
        let res = match op {
            BinOp::Add => li.wrapping_add(ri),
            BinOp::Sub => li.wrapping_sub(ri),
            BinOp::Mul => li.wrapping_mul(ri),
            _ => unreachable!(),
        };
        Ok(Value::BigInt(res))
    }
}

fn arith_neg(v: Value) -> R<Value> {
    Ok(match v {
        Value::Null => Value::Null,
        Value::Int(i) => Value::Int(-i),
        Value::BigInt(i) => Value::BigInt(-i),
        Value::Double(d) => Value::Double(-d),
        _ => return err("unary minus on non-numeric"),
    })
}

fn compare(op: BinOp, l: Value, r: Value) -> R<Value> {
    if l.is_null() || r.is_null() {
        return Ok(Value::Null);
    }
    let ord = cmp_nonnull(&l, &r)?;
    Ok(Value::Bool(match op {
        BinOp::Eq => ord == Ordering::Equal,
        BinOp::Ne => ord != Ordering::Equal,
        BinOp::Lt => ord == Ordering::Less,
        BinOp::Le => ord != Ordering::Greater,
        BinOp::Gt => ord == Ordering::Greater,
        BinOp::Ge => ord != Ordering::Less,
        _ => unreachable!(),
    }))
}

fn cmp_nonnull(l: &Value, r: &Value) -> R<Ordering> {
    if let (Some((lf, _)), Some((rf, _))) = (num(l), num(r)) {
        return Ok(lf.partial_cmp(&rf).unwrap_or(Ordering::Equal));
    }
    match (l, r) {
        (Value::Text(a), Value::Text(b)) => Ok(a.cmp(b)),
        (Value::Bool(a), Value::Bool(b)) => Ok(a.cmp(b)),
        _ => err(format!("cannot compare {l:?} with {r:?}")),
    }
}

fn in_result<'a>(v: &Value, items: impl Iterator<Item = &'a Value>, negated: bool) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    let mut saw_null = false;
    for it in items {
        if it.is_null() {
            saw_null = true;
            continue;
        }
        if cmp_nonnull(v, it)
            .map(|o| o == Ordering::Equal)
            .unwrap_or(false)
        {
            return Value::Bool(!negated);
        }
    }
    if saw_null {
        Value::Null
    } else {
        Value::Bool(negated)
    }
}

fn sum_values(vals: &[Value]) -> R<Value> {
    let any_float = vals.iter().any(|v| matches!(v, Value::Double(_)));
    if any_float {
        Ok(Value::Double(to_f64_sum(vals)?))
    } else {
        let mut s: i64 = 0;
        for v in vals {
            let (f, _) = num(v).ok_or_else(|| ExecError("SUM of non-numeric".into()))?;
            s = s.wrapping_add(f as i64);
        }
        Ok(Value::BigInt(s))
    }
}
fn to_f64_sum(vals: &[Value]) -> R<f64> {
    let mut s = 0.0;
    for v in vals {
        let (f, _) = num(v).ok_or_else(|| ExecError("numeric aggregate of non-numeric".into()))?;
        s += f;
    }
    Ok(s)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct OrdVal(Value);
impl PartialOrd for OrdVal {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OrdVal {
    fn cmp(&self, other: &Self) -> Ordering {
        if let (Some((a, _)), Some((b, _))) = (num(&self.0), num(&other.0)) {
            a.partial_cmp(&b).unwrap_or(Ordering::Equal)
        } else {
            self.0.total_cmp(&other.0)
        }
    }
}

fn dedup(rows: Vec<Row>) -> Vec<Row> {
    let mut seen: BTreeMap<Vec<OrdVal>, ()> = BTreeMap::new();
    let mut out = Vec::new();
    for r in rows {
        let key: Vec<OrdVal> = r.iter().cloned().map(OrdVal).collect();
        if seen.insert(key, ()).is_none() {
            out.push(r);
        }
    }
    out
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

fn expr_label(e: &Expr) -> String {
    match e {
        Expr::Column { name, .. } => name.clone(),
        Expr::Aggregate { func, .. } => format!("{func:?}").to_lowercase(),
        _ => "?column?".to_string(),
    }
}

fn coerce(v: Value, ty: ColumnType) -> R<Value> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    Ok(match (ty, &v) {
        (ColumnType::Bool, Value::Bool(_)) => v,
        (ColumnType::Int, Value::BigInt(n)) => Value::Int(*n as i32),
        (ColumnType::Int, Value::Int(_)) => v,
        (ColumnType::BigInt, Value::BigInt(_)) => v,
        (ColumnType::BigInt, Value::Int(n)) => Value::BigInt(*n as i64),
        (ColumnType::Double, Value::Double(_)) => v,
        (ColumnType::Double, Value::BigInt(n)) => Value::Double(*n as f64),
        (ColumnType::Double, Value::Int(n)) => Value::Double(*n as f64),
        (ColumnType::Varchar(_), Value::Text(_)) => v,
        _ => return err(format!("cannot store {v:?} into {ty:?}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_statement;

    fn run(db: &mut MemDb, sql: &str) -> Option<ResultSet> {
        let stmt = parse_statement(sql).unwrap();
        db.execute(&stmt).unwrap()
    }

    fn setup() -> MemDb {
        let mut db = MemDb::new();
        run(
            &mut db,
            "CREATE TABLE t (id BIGINT, name VARCHAR(16), x INT, p DOUBLE)",
        );
        run(
            &mut db,
            "INSERT INTO t VALUES (1,'a',10,1.5),(2,'b',20,2.5),(3,'a',30,NULL),(4,'c',NULL,4.5)",
        );
        db
    }

    #[test]
    fn select_star_and_where() {
        let mut db = setup();
        let rs = run(&mut db, "SELECT id, name FROM t WHERE x > 15").unwrap();
        assert_eq!(rs.columns, vec!["id", "name"]);
        assert_eq!(rs.rows.len(), 2);
    }

    #[test]
    fn three_valued_where_drops_null() {
        let mut db = setup();
        assert_eq!(
            run(&mut db, "SELECT id FROM t WHERE x = NULL")
                .unwrap()
                .rows
                .len(),
            0
        );
        assert_eq!(
            run(&mut db, "SELECT id FROM t WHERE x IS NULL")
                .unwrap()
                .rows
                .len(),
            1
        );
        assert_eq!(
            run(&mut db, "SELECT id FROM t WHERE NOT (x > 100)")
                .unwrap()
                .rows
                .len(),
            3
        );
    }

    #[test]
    fn aggregates_and_group_by() {
        let mut db = setup();
        let rs = run(
            &mut db,
            "SELECT name, COUNT(*), SUM(x) FROM t GROUP BY name ORDER BY name",
        )
        .unwrap();
        assert_eq!(rs.rows.len(), 3);
        assert_eq!(rs.rows[0][1], Value::BigInt(2));
        assert_eq!(rs.rows[0][2], Value::BigInt(40));
        assert_eq!(rs.rows[2][2], Value::Null);
    }

    #[test]
    fn having_and_count() {
        let mut db = setup();
        let rs = run(
            &mut db,
            "SELECT name FROM t GROUP BY name HAVING COUNT(*) > 1",
        )
        .unwrap();
        assert_eq!(rs.rows, vec![vec![Value::Text("a".into())]]);
    }

    #[test]
    fn inner_and_left_join() {
        let mut db = MemDb::new();
        run(&mut db, "CREATE TABLE a (id INT, v INT)");
        run(&mut db, "CREATE TABLE b (id INT, w INT)");
        run(&mut db, "INSERT INTO a VALUES (1,10),(2,20),(3,30)");
        run(&mut db, "INSERT INTO b VALUES (1,100),(2,200)");
        let inner = run(&mut db, "SELECT a.id FROM a JOIN b ON a.id = b.id").unwrap();
        assert_eq!(inner.rows.len(), 2);
        let left = run(
            &mut db,
            "SELECT a.id, b.w FROM a LEFT JOIN b ON a.id = b.id ORDER BY id",
        )
        .unwrap();
        assert_eq!(left.rows.len(), 3);
        assert_eq!(left.rows[2][1], Value::Null);
    }

    #[test]
    fn case_in_and_scalar_subquery() {
        let mut db = setup();
        let rs = run(
            &mut db,
            "SELECT id, CASE WHEN x >= 20 THEN 'hi' ELSE 'lo' END FROM t WHERE id IN (1,2)",
        )
        .unwrap();
        assert_eq!(rs.rows.len(), 2);
        assert_eq!(rs.rows[0][1], Value::Text("lo".into()));
        assert_eq!(rs.rows[1][1], Value::Text("hi".into()));

        let sq = run(&mut db, "SELECT id FROM t WHERE x = (SELECT MAX(x) FROM t)").unwrap();
        assert_eq!(sq.rows, vec![vec![Value::BigInt(3)]]);
    }

    #[test]
    fn delete_with_and_without_where() {
        let mut db = setup();
        run(&mut db, "DELETE FROM t WHERE x > 15");
        let rs = run(&mut db, "SELECT id FROM t ORDER BY id").unwrap();
        assert_eq!(
            rs.rows,
            vec![vec![Value::BigInt(1)], vec![Value::BigInt(4)]]
        );
        run(&mut db, "DELETE FROM t");
        assert_eq!(run(&mut db, "SELECT id FROM t").unwrap().rows.len(), 0);
    }

    #[test]
    fn update_sees_pre_update_row() {
        let mut db = setup();
        run(&mut db, "UPDATE t SET x = x + 100 WHERE id = 1");
        let rs = run(&mut db, "SELECT x FROM t WHERE id = 1").unwrap();
        assert_eq!(rs.rows, vec![vec![Value::Int(110)]]);
        run(&mut db, "UPDATE t SET x = 5, p = x WHERE id = 2");
        let rs = run(&mut db, "SELECT x, p FROM t WHERE id = 2").unwrap();
        assert_eq!(rs.rows[0][0], Value::Int(5));
        assert_eq!(rs.rows[0][1], Value::Double(20.0));
    }

    #[test]
    fn update_all_rows_without_where() {
        let mut db = setup();
        run(&mut db, "UPDATE t SET name = 'z'");
        let rs = run(&mut db, "SELECT DISTINCT name FROM t").unwrap();
        assert_eq!(rs.rows, vec![vec![Value::Text("z".into())]]);
    }

    #[test]
    fn distinct_and_limit() {
        let mut db = setup();
        let rs = run(&mut db, "SELECT DISTINCT name FROM t ORDER BY name").unwrap();
        assert_eq!(rs.rows.len(), 3);
        let lim = run(&mut db, "SELECT id FROM t ORDER BY id LIMIT 2").unwrap();
        assert_eq!(
            lim.rows,
            vec![vec![Value::BigInt(1)], vec![Value::BigInt(2)]]
        );
    }
}
