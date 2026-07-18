//! Normalized keys (D9): every index key is a `memcmp`-comparable byte string.
//!
//! Encoding a typed tuple to bytes whose lexicographic order equals the tuple's
//! logical order is the single decision that makes the B-tree type-oblivious and
//! deletes a whole family of comparator bugs. The rules:
//!
//!   * **Presence tag** — each field is prefixed with `0x00` (NULL) or `0x01`
//!     (present), so NULL sorts below every value (NULL-low) and composite keys
//!     parse unambiguously.
//!   * **Integers** — big-endian with the sign bit flipped, so negatives sort
//!     below positives under unsigned byte compare.
//!   * **Doubles** — the IEEE total-order transform (shared with `keel-types`),
//!     big-endian.
//!   * **Bool** — one byte, `0x00 < 0x01`.
//!   * **Strings** — order-preserving escape: `0x00 -> 0x00 0xFF`, terminated by
//!     `0x00 0x00`. A prefix sorts before its extensions; embedded NULs are
//!     safe.
//!   * **Composite** — concatenation. Because every field encoding is either
//!     fixed-width or self-terminating, concatenation preserves tuple order and
//!     stays decodable.
//!
//! The property test at the bottom is the whole point: for random typed values,
//! sorting by encoded bytes must equal sorting by [`keel_types::Value`] order.

use keel_types::{f64_total_order_bits, ColumnType, Value};

const TAG_NULL: u8 = 0x00;
const TAG_PRESENT: u8 = 0x01;

const STR_ESC: u8 = 0x00;
const STR_ESC_MARK: u8 = 0xFF;
const STR_TERM: u8 = 0x00;

/// Append a single value's normalized encoding to `out`.
pub fn encode_value_into(out: &mut Vec<u8>, ty: ColumnType, v: &Value) {
    if v.is_null() {
        out.push(TAG_NULL);
        return;
    }
    out.push(TAG_PRESENT);
    match (ty, v) {
        (ColumnType::Bool, Value::Bool(b)) => out.push(*b as u8),
        (ColumnType::Int, Value::Int(i)) => {
            let u = (*i as u32) ^ 0x8000_0000;
            out.extend_from_slice(&u.to_be_bytes());
        }
        (ColumnType::BigInt, Value::BigInt(i)) => {
            let u = (*i as u64) ^ 0x8000_0000_0000_0000;
            out.extend_from_slice(&u.to_be_bytes());
        }
        (ColumnType::Double, Value::Double(d)) => {
            out.extend_from_slice(&f64_total_order_bits(*d).to_be_bytes());
        }
        (ColumnType::Varchar(_), Value::Text(s)) => {
            for &b in s.as_bytes() {
                if b == STR_ESC {
                    out.push(STR_ESC);
                    out.push(STR_ESC_MARK);
                } else {
                    out.push(b);
                }
            }
            out.push(STR_TERM);
            out.push(0x00);
        }
        (ty, v) => panic!("encode_value: type {ty:?} does not match value {v:?}"),
    }
}

/// Encode a single value to its own key bytes.
pub fn encode_value(ty: ColumnType, v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    encode_value_into(&mut out, ty, v);
    out
}

/// Encode a composite key from a typed tuple. Panics if arities disagree.
pub fn encode_key(types: &[ColumnType], values: &[Value]) -> Vec<u8> {
    assert_eq!(types.len(), values.len(), "key arity mismatch");
    let mut out = Vec::new();
    for (ty, v) in types.iter().zip(values) {
        encode_value_into(&mut out, *ty, v);
    }
    out
}

/// Errors from key decoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyError {
    Truncated,
    BadTag(u8),
    BadStringEscape,
    NotUtf8,
}

/// Decode a composite key back into values (for B-tree separators, `dbcheck`,
/// and `pageview`).
pub fn decode_key(types: &[ColumnType], mut bytes: &[u8]) -> Result<Vec<Value>, KeyError> {
    let mut out = Vec::with_capacity(types.len());
    for &ty in types {
        let v = decode_value(&mut bytes, ty)?;
        out.push(v);
    }
    Ok(out)
}

fn decode_value(bytes: &mut &[u8], ty: ColumnType) -> Result<Value, KeyError> {
    let (&tag, rest) = bytes.split_first().ok_or(KeyError::Truncated)?;
    *bytes = rest;
    match tag {
        TAG_NULL => Ok(Value::Null),
        TAG_PRESENT => decode_present(bytes, ty),
        other => Err(KeyError::BadTag(other)),
    }
}

fn take<'a>(bytes: &mut &'a [u8], n: usize) -> Result<&'a [u8], KeyError> {
    if bytes.len() < n {
        return Err(KeyError::Truncated);
    }
    let (head, tail) = bytes.split_at(n);
    *bytes = tail;
    Ok(head)
}

fn decode_present(bytes: &mut &[u8], ty: ColumnType) -> Result<Value, KeyError> {
    Ok(match ty {
        ColumnType::Bool => {
            let b = take(bytes, 1)?;
            Value::Bool(b[0] != 0)
        }
        ColumnType::Int => {
            let b = take(bytes, 4)?;
            let u = u32::from_be_bytes(b.try_into().unwrap()) ^ 0x8000_0000;
            Value::Int(u as i32)
        }
        ColumnType::BigInt => {
            let b = take(bytes, 8)?;
            let u = u64::from_be_bytes(b.try_into().unwrap()) ^ 0x8000_0000_0000_0000;
            Value::BigInt(u as i64)
        }
        ColumnType::Double => {
            let b = take(bytes, 8)?;
            let ordered = u64::from_be_bytes(b.try_into().unwrap());
            Value::Double(f64_from_total_order_bits(ordered))
        }
        ColumnType::Varchar(_) => {
            let mut raw = Vec::new();
            loop {
                let (&b, rest) = bytes.split_first().ok_or(KeyError::Truncated)?;
                *bytes = rest;
                if b == STR_ESC {
                    let (&next, rest2) = bytes.split_first().ok_or(KeyError::Truncated)?;
                    *bytes = rest2;
                    match next {
                        STR_ESC_MARK => raw.push(0x00),
                        0x00 => break,
                        _ => return Err(KeyError::BadStringEscape),
                    }
                } else {
                    raw.push(b);
                }
            }
            Value::Text(String::from_utf8(raw).map_err(|_| KeyError::NotUtf8)?)
        }
    })
}

/// Inverse of [`keel_types::f64_total_order_bits`].
fn f64_from_total_order_bits(ordered: u64) -> f64 {
    let bits = if ordered & 0x8000_0000_0000_0000 != 0 {
        ordered ^ 0x8000_0000_0000_0000
    } else {
        !ordered
    };
    f64::from_bits(bits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_rng::Rng;

    #[test]
    fn f64_bits_roundtrip() {
        for x in [
            0.0f64,
            -0.0,
            1.0,
            -1.0,
            f64::MIN,
            f64::MAX,
            f64::INFINITY,
            f64::NEG_INFINITY,
            std::f64::consts::PI,
            -std::f64::consts::E,
        ] {
            let back = f64_from_total_order_bits(f64_total_order_bits(x));
            assert_eq!(back.to_bits(), x.to_bits(), "roundtrip {x}");
        }
    }

    #[test]
    fn single_value_roundtrip() {
        let cases: Vec<(ColumnType, Value)> = vec![
            (ColumnType::Bool, Value::Bool(true)),
            (ColumnType::Bool, Value::Bool(false)),
            (ColumnType::Int, Value::Int(0)),
            (ColumnType::Int, Value::Int(-1)),
            (ColumnType::Int, Value::Int(i32::MIN)),
            (ColumnType::Int, Value::Int(i32::MAX)),
            (ColumnType::BigInt, Value::BigInt(-9_000_000_000)),
            (ColumnType::Double, Value::Double(-0.5)),
            (ColumnType::Varchar(64), Value::Text(String::new())),
            (ColumnType::Varchar(64), Value::Text("hello".into())),
            (ColumnType::Varchar(64), Value::Text("with\0nul".into())),
            (ColumnType::Int, Value::Null),
        ];
        for (ty, v) in cases {
            let enc = encode_value(ty, &v);
            let back = decode_key(&[ty], &enc).unwrap();
            assert_eq!(back, vec![v.clone()], "roundtrip {v:?}");
        }
    }

    #[test]
    fn composite_roundtrip() {
        let types = [ColumnType::Int, ColumnType::Varchar(16), ColumnType::BigInt];
        let vals = vec![Value::Int(7), Value::Text("abc".into()), Value::BigInt(-3)];
        let enc = encode_key(&types, &vals);
        assert_eq!(decode_key(&types, &enc).unwrap(), vals);
    }

    fn assert_order_preserved(ty: ColumnType, mut vals: Vec<Value>) {
        let mut by_value = vals.clone();
        by_value.sort();
        vals.sort_by_key(|v| encode_value(ty, v));
        assert_eq!(by_value, vals, "order not preserved for {ty:?}");
    }

    #[test]
    fn order_preserved_ints() {
        let mut r = Rng::seed(1);
        let mut vals = vec![Value::Null];
        for _ in 0..500 {
            vals.push(Value::Int(r.next_u32() as i32));
        }
        assert_order_preserved(ColumnType::Int, vals);
    }

    #[test]
    fn order_preserved_bigints() {
        let mut r = Rng::seed(2);
        let mut vals = vec![Value::Null];
        for _ in 0..500 {
            vals.push(Value::BigInt(r.next_u64() as i64));
        }
        assert_order_preserved(ColumnType::BigInt, vals);
    }

    #[test]
    fn order_preserved_doubles() {
        let mut r = Rng::seed(3);
        let mut vals = vec![Value::Null];
        for _ in 0..500 {
            let bits = r.next_u64();
            let d = f64::from_bits(bits);
            if d.is_nan() {
                continue;
            }
            vals.push(Value::Double(d));
        }
        vals.push(Value::Double(f64::INFINITY));
        vals.push(Value::Double(f64::NEG_INFINITY));
        vals.push(Value::Double(0.0));
        vals.push(Value::Double(-0.0));
        assert_order_preserved(ColumnType::Double, vals);
    }

    #[test]
    fn order_preserved_strings_incl_adversarial() {
        let mut r = Rng::seed(4);
        let mut vals = vec![
            Value::Null,
            Value::Text(String::new()),
            Value::Text("\0".into()),
            Value::Text("\0\0".into()),
            Value::Text("a".into()),
            Value::Text("a\0".into()),
            Value::Text("ab".into()),
        ];
        for _ in 0..500 {
            let len = r.below(10) as usize;
            let s: String = (0..len).map(|_| (r.below(4) as u8) as char).collect();
            vals.push(Value::Text(s));
        }
        assert_order_preserved(ColumnType::Varchar(64), vals);
    }

    #[test]
    fn composite_order_preserved() {
        let mut r = Rng::seed(5);
        let types = [ColumnType::Int, ColumnType::Varchar(8)];
        let mut tuples: Vec<Vec<Value>> = Vec::new();
        for _ in 0..800 {
            let a = if r.one_in(6) {
                Value::Null
            } else {
                Value::Int((r.below(5) as i32) - 2)
            };
            let b = if r.one_in(6) {
                Value::Null
            } else {
                let len = r.below(4) as usize;
                Value::Text(
                    (0..len)
                        .map(|_| (b'a' + r.below(3) as u8) as char)
                        .collect(),
                )
            };
            tuples.push(vec![a, b]);
        }
        let mut by_value = tuples.clone();
        by_value.sort();
        tuples.sort_by_key(|t| encode_key(&types, t));
        assert_eq!(by_value, tuples, "composite order not preserved");
    }
}
