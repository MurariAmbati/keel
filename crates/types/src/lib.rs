use std::cmp::Ordering;
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnType {
    Bool,
    Int,
    BigInt,
    Double,
    Varchar(u16),
}

impl ColumnType {
    pub fn fixed_width(self) -> Option<usize> {
        match self {
            ColumnType::Bool => Some(1),
            ColumnType::Int => Some(4),
            ColumnType::BigInt => Some(8),
            ColumnType::Double => Some(8),
            ColumnType::Varchar(_) => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ColumnType::Bool => "bool",
            ColumnType::Int => "int",
            ColumnType::BigInt => "bigint",
            ColumnType::Double => "double",
            ColumnType::Varchar(_) => "varchar",
        }
    }
}

#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i32),
    BigInt(i64),
    Double(f64),
    Text(String),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn type_of(&self) -> Option<ColumnType> {
        Some(match self {
            Value::Null => return None,
            Value::Bool(_) => ColumnType::Bool,
            Value::Int(_) => ColumnType::Int,
            Value::BigInt(_) => ColumnType::BigInt,
            Value::Double(_) => ColumnType::Double,
            Value::Text(_) => ColumnType::Varchar(u16::MAX),
        })
    }

    pub fn fits(&self, ty: ColumnType) -> bool {
        match (self, ty) {
            (Value::Null, _) => true,
            (Value::Bool(_), ColumnType::Bool) => true,
            (Value::Int(_), ColumnType::Int) => true,
            (Value::BigInt(_), ColumnType::BigInt) => true,
            (Value::Double(_), ColumnType::Double) => true,
            (Value::Text(s), ColumnType::Varchar(n)) => s.len() <= n as usize,
            _ => false,
        }
    }
}

pub fn f64_total_order_bits(x: f64) -> u64 {
    let bits = x.to_bits();
    if bits & 0x8000_0000_0000_0000 != 0 {
        !bits
    } else {
        bits ^ 0x8000_0000_0000_0000
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        self.total_cmp(other) == Ordering::Equal
    }
}
impl Eq for Value {}
impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Value {
    fn cmp(&self, other: &Self) -> Ordering {
        self.total_cmp(other)
    }
}

impl Value {
    pub fn total_cmp(&self, other: &Value) -> Ordering {
        fn rank(v: &Value) -> u8 {
            match v {
                Value::Null => 0,
                Value::Bool(_) => 1,
                Value::Int(_) => 2,
                Value::BigInt(_) => 3,
                Value::Double(_) => 4,
                Value::Text(_) => 5,
            }
        }
        match (self, other) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
            (Value::Int(a), Value::Int(b)) => a.cmp(b),
            (Value::BigInt(a), Value::BigInt(b)) => a.cmp(b),
            (Value::Double(a), Value::Double(b)) => {
                f64_total_order_bits(*a).cmp(&f64_total_order_bits(*b))
            }
            (Value::Text(a), Value::Text(b)) => a.as_bytes().cmp(b.as_bytes()),
            _ => rank(self).cmp(&rank(other)),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::BigInt(i) => write!(f, "{i}"),
            Value::Double(d) => write!(f, "{d}"),
            Value::Text(s) => write!(f, "{s}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnDef {
    pub name: String,
    pub ty: ColumnType,
    pub not_null: bool,
}

impl ColumnDef {
    pub fn new(name: impl Into<String>, ty: ColumnType, not_null: bool) -> Self {
        Self {
            name: name.into(),
            ty,
            not_null,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Schema {
    pub columns: Vec<ColumnDef>,
}

impl Schema {
    pub fn new(columns: Vec<ColumnDef>) -> Self {
        Self { columns }
    }

    pub fn len(&self) -> usize {
        self.columns.len()
    }
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordError {
    Arity { expected: usize, got: usize },
    TypeMismatch { column: usize },
    NullViolation { column: usize },
    TooLong { column: usize },
    Truncated,
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecordError::Arity { expected, got } => {
                write!(f, "row arity {got} != schema arity {expected}")
            }
            RecordError::TypeMismatch { column } => write!(f, "type mismatch at column {column}"),
            RecordError::NullViolation { column } => write!(f, "NULL in NOT NULL column {column}"),
            RecordError::TooLong { column } => write!(f, "varchar too long at column {column}"),
            RecordError::Truncated => write!(f, "record bytes truncated"),
        }
    }
}
impl std::error::Error for RecordError {}

fn bitmap_len(ncols: usize) -> usize {
    ncols.div_ceil(8)
}

pub fn encode_record(schema: &Schema, row: &[Value]) -> Result<Vec<u8>, RecordError> {
    if row.len() != schema.columns.len() {
        return Err(RecordError::Arity {
            expected: schema.columns.len(),
            got: row.len(),
        });
    }
    let nb = bitmap_len(schema.columns.len());
    let mut out = vec![0u8; nb];
    for (i, (col, val)) in schema.columns.iter().zip(row).enumerate() {
        if val.is_null() {
            if col.not_null {
                return Err(RecordError::NullViolation { column: i });
            }
            out[i / 8] |= 1 << (i % 8);
            continue;
        }
        if !val.fits(col.ty) {
            if let (Value::Text(s), ColumnType::Varchar(n)) = (val, col.ty) {
                if s.len() > n as usize {
                    return Err(RecordError::TooLong { column: i });
                }
            }
            return Err(RecordError::TypeMismatch { column: i });
        }
        match val {
            Value::Bool(b) => out.push(*b as u8),
            Value::Int(v) => out.extend_from_slice(&v.to_le_bytes()),
            Value::BigInt(v) => out.extend_from_slice(&v.to_le_bytes()),
            Value::Double(v) => out.extend_from_slice(&v.to_bits().to_le_bytes()),
            Value::Text(s) => {
                out.extend_from_slice(&(s.len() as u16).to_le_bytes());
                out.extend_from_slice(s.as_bytes());
            }
            Value::Null => unreachable!("handled above"),
        }
    }
    Ok(out)
}

pub fn decode_record(schema: &Schema, bytes: &[u8]) -> Result<Vec<Value>, RecordError> {
    let nb = bitmap_len(schema.columns.len());
    if bytes.len() < nb {
        return Err(RecordError::Truncated);
    }
    let (bitmap, mut body) = bytes.split_at(nb);
    let mut row = Vec::with_capacity(schema.columns.len());
    for (i, col) in schema.columns.iter().enumerate() {
        let is_null = bitmap[i / 8] & (1 << (i % 8)) != 0;
        if is_null {
            row.push(Value::Null);
            continue;
        }
        let v = match col.ty {
            ColumnType::Bool => {
                let b = take(&mut body, 1)?;
                Value::Bool(b[0] != 0)
            }
            ColumnType::Int => {
                let b = take(&mut body, 4)?;
                Value::Int(i32::from_le_bytes(b.try_into().unwrap()))
            }
            ColumnType::BigInt => {
                let b = take(&mut body, 8)?;
                Value::BigInt(i64::from_le_bytes(b.try_into().unwrap()))
            }
            ColumnType::Double => {
                let b = take(&mut body, 8)?;
                Value::Double(f64::from_bits(u64::from_le_bytes(b.try_into().unwrap())))
            }
            ColumnType::Varchar(_) => {
                let lb = take(&mut body, 2)?;
                let len = u16::from_le_bytes(lb.try_into().unwrap()) as usize;
                let sb = take(&mut body, len)?;
                Value::Text(String::from_utf8(sb.to_vec()).map_err(|_| RecordError::Truncated)?)
            }
        };
        row.push(v);
    }
    Ok(row)
}

fn take<'a>(body: &mut &'a [u8], n: usize) -> Result<&'a [u8], RecordError> {
    if body.len() < n {
        return Err(RecordError::Truncated);
    }
    let (head, tail) = body.split_at(n);
    *body = tail;
    Ok(head)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_schema() -> Schema {
        Schema::new(vec![
            ColumnDef::new("id", ColumnType::BigInt, true),
            ColumnDef::new("age", ColumnType::Int, false),
            ColumnDef::new("score", ColumnType::Double, false),
            ColumnDef::new("active", ColumnType::Bool, false),
            ColumnDef::new("name", ColumnType::Varchar(32), false),
        ])
    }

    #[test]
    fn record_roundtrip_all_present() {
        let s = sample_schema();
        let row = vec![
            Value::BigInt(42),
            Value::Int(-7),
            Value::Double(3.5),
            Value::Bool(true),
            Value::Text("keel".into()),
        ];
        let bytes = encode_record(&s, &row).unwrap();
        let back = decode_record(&s, &bytes).unwrap();
        assert_eq!(back, row);
    }

    #[test]
    fn record_roundtrip_with_nulls() {
        let s = sample_schema();
        let row = vec![
            Value::BigInt(1),
            Value::Null,
            Value::Null,
            Value::Bool(false),
            Value::Null,
        ];
        let bytes = encode_record(&s, &row).unwrap();
        let back = decode_record(&s, &bytes).unwrap();
        assert_eq!(back, row);
    }

    #[test]
    fn not_null_violation_rejected() {
        let s = sample_schema();
        let row = vec![
            Value::Null,
            Value::Int(1),
            Value::Double(0.0),
            Value::Bool(true),
            Value::Text("x".into()),
        ];
        assert_eq!(
            encode_record(&s, &row),
            Err(RecordError::NullViolation { column: 0 })
        );
    }

    #[test]
    fn varchar_length_enforced() {
        let s = Schema::new(vec![ColumnDef::new("n", ColumnType::Varchar(3), false)]);
        assert_eq!(
            encode_record(&s, &[Value::Text("toolong".into())]),
            Err(RecordError::TooLong { column: 0 })
        );
        assert!(encode_record(&s, &[Value::Text("ok".into())]).is_ok());
    }

    #[test]
    fn double_total_order_is_sane() {
        let mut xs = [
            Value::Double(f64::NEG_INFINITY),
            Value::Double(-1.0),
            Value::Double(-0.0),
            Value::Double(0.0),
            Value::Double(1.0),
            Value::Double(f64::INFINITY),
        ];
        xs.sort();
        assert_eq!(
            xs,
            [
                Value::Double(f64::NEG_INFINITY),
                Value::Double(-1.0),
                Value::Double(-0.0),
                Value::Double(0.0),
                Value::Double(1.0),
                Value::Double(f64::INFINITY),
            ]
        );
    }
}
