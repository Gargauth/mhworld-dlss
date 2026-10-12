//! Memory expressions and typed values (`mem.read`, `mem.write`, `watch`).
//!
//! An expression is a base (a symbol from `symbols`, an alias, or an
//! absolute address) plus an offset and an optional type: as text
//! `cfg+0x200:f32`, `0x1451c4480+8:u32`, `Camera:mat4`, or as the params
//! object `{"symbol" | "addr", "offset", "as"}` the client sends. Addresses
//! travel as hex strings (64-bit values exceed a double).

use serde_json::{Map, Value};

use crate::proto::{RpcError, RpcResult};

/// How memory is viewed. Little-endian throughout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ValueType {
    /// `u8`.
    U8,
    /// `u16`.
    U16,
    /// `u32`.
    U32,
    /// `u64`.
    U64,
    /// `f32`.
    F32,
    /// `f64`.
    F64,
    /// 16 `f32` in memory order (row-major in this engine).
    Mat4,
    /// Raw bytes.
    Bytes,
}

impl ValueType {
    const NAMES: [(&'static str, ValueType); 8] = [
        ("u8", ValueType::U8),
        ("u16", ValueType::U16),
        ("u32", ValueType::U32),
        ("u64", ValueType::U64),
        ("f32", ValueType::F32),
        ("f64", ValueType::F64),
        ("mat4", ValueType::Mat4),
        ("bytes", ValueType::Bytes),
    ];

    /// The type called `name`.
    pub fn parse(name: &str) -> Option<ValueType> {
        Self::NAMES
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, t)| *t)
    }

    /// The wire name.
    pub fn name(self) -> &'static str {
        Self::NAMES
            .iter()
            .find(|(_, t)| *t == self)
            .map_or("bytes", |(n, _)| n)
    }

    /// Bytes per element (`bytes`: 1).
    pub const fn size(self) -> usize {
        match self {
            ValueType::U8 | ValueType::Bytes => 1,
            ValueType::U16 => 2,
            ValueType::U32 | ValueType::F32 => 4,
            ValueType::U64 | ValueType::F64 => 8,
            ValueType::Mat4 => 64,
        }
    }

    fn unknown(name: &str) -> RpcError {
        let names: Vec<&str> = Self::NAMES.iter().map(|(n, _)| *n).collect();
        RpcError::bad_request(format!(
            "unknown type {name:?} (one of {})",
            names.join(" ")
        ))
    }
}

fn f32_at(bytes: &[u8], at: usize) -> f32 {
    let mut word = [0u8; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    f32::from_le_bytes(word)
}

/// Decode `bytes` as consecutive elements of `ty`; a trailing partial
/// element is ignored. Non-finite floats become `null` (JSON has no NaN).
pub fn decode(bytes: &[u8], ty: ValueType) -> Vec<Value> {
    let size = ty.size();
    bytes
        .chunks_exact(size)
        .map(|c| match ty {
            ValueType::U8 | ValueType::Bytes => Value::from(c[0]),
            ValueType::U16 => Value::from(u16::from_le_bytes([c[0], c[1]])),
            ValueType::U32 => Value::from(u32::from_le_bytes([c[0], c[1], c[2], c[3]])),
            ValueType::U64 => {
                let mut word = [0u8; 8];
                word.copy_from_slice(c);
                Value::from(u64::from_le_bytes(word))
            }
            ValueType::F32 => float(f64::from(f32_at(c, 0))),
            ValueType::F64 => {
                let mut word = [0u8; 8];
                word.copy_from_slice(c);
                float(f64::from_le_bytes(word))
            }
            ValueType::Mat4 => Value::Array(
                (0..16)
                    .map(|i| float(f64::from(f32_at(c, i * 4))))
                    .collect(),
            ),
        })
        .collect()
}

/// A JSON number, or `null` when not finite.
pub fn float(x: f64) -> Value {
    serde_json::Number::from_f64(x).map_or(Value::Null, Value::Number)
}

fn uint(value: &Value, max: u64) -> RpcResult<u64> {
    let n = match value {
        Value::Number(n) => n.as_u64().or_else(|| {
            n.as_f64()
                .filter(|f| f.fract() == 0.0 && *f >= 0.0 && *f <= u64::MAX as f64)
                .map(|f| f as u64)
        }),
        _ => None,
    };
    n.filter(|&n| n <= max)
        .ok_or_else(|| RpcError::bad_request(format!("{value} is not an integer 0..={max}")))
}

fn number(value: &Value) -> RpcResult<f64> {
    value
        .as_f64()
        .ok_or_else(|| RpcError::bad_request(format!("{value} is not a number")))
}

/// Encode `value` as `ty`: a number, a list of numbers, 16 numbers (or a
/// list of 16-number lists) for `mat4`, a hex string for `bytes`.
pub fn encode(value: &Value, ty: ValueType) -> RpcResult<Vec<u8>> {
    let mut out = Vec::new();
    match ty {
        ValueType::Bytes => {
            let text = value
                .as_str()
                .ok_or_else(|| RpcError::bad_request("a bytes value is a hex string"))?;
            out = parse_hex(text)
                .ok_or_else(|| RpcError::bad_request("bytes: invalid hex string"))?;
        }
        ValueType::Mat4 => {
            let items = value
                .as_array()
                .ok_or_else(|| RpcError::bad_request("mat4 needs 16 numbers"))?;
            let matrices: Vec<&Vec<Value>> = if items.first().is_some_and(Value::is_array) {
                items.iter().filter_map(Value::as_array).collect()
            } else {
                vec![items]
            };
            if matrices.len() != items.len() && items.first().is_some_and(Value::is_array) {
                return Err(RpcError::bad_request("mat4: mixed numbers and lists"));
            }
            for m in matrices {
                if m.len() != 16 {
                    return Err(RpcError::bad_request(format!(
                        "mat4 needs 16 numbers, got {}",
                        m.len()
                    )));
                }
                for v in m {
                    out.extend_from_slice(&(number(v)? as f32).to_le_bytes());
                }
            }
        }
        _ => {
            let items = match value {
                Value::Array(items) => items.as_slice(),
                single => std::slice::from_ref(single),
            };
            if items.is_empty() {
                return Err(RpcError::bad_request("no value to write"));
            }
            for v in items {
                match ty {
                    ValueType::U8 => out.push(uint(v, u8::MAX.into())? as u8),
                    ValueType::U16 => {
                        out.extend_from_slice(&(uint(v, u16::MAX.into())? as u16).to_le_bytes())
                    }
                    ValueType::U32 => {
                        out.extend_from_slice(&(uint(v, u32::MAX.into())? as u32).to_le_bytes())
                    }
                    ValueType::U64 => out.extend_from_slice(&uint(v, u64::MAX)?.to_le_bytes()),
                    ValueType::F32 => out.extend_from_slice(&(number(v)? as f32).to_le_bytes()),
                    _ => out.extend_from_slice(&number(v)?.to_le_bytes()),
                }
            }
        }
    }
    if out.is_empty() {
        return Err(RpcError::bad_request("no value to write"));
    }
    Ok(out)
}

fn parse_hex(text: &str) -> Option<Vec<u8>> {
    let digits: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let (pairs, rest) = digits.as_chunks::<2>();
    if !rest.is_empty() {
        return None;
    }
    pairs
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect()
}

/// `0x…` or decimal.
pub fn parse_uint(text: &str) -> Option<u64> {
    let text = text.trim();
    match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => text.parse().ok(),
    }
}

/// An address as it travels: `"0x…"`.
pub fn hex(address: u64) -> Value {
    Value::String(format!("{address:#x}"))
}

/// What an expression is relative to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Base {
    /// A name from the address book or an alias.
    Symbol(String),
    /// An absolute address.
    Addr(u64),
}

/// A memory location with an optional type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expr {
    /// What it is relative to.
    pub base: Base,
    /// Signed byte offset.
    pub offset: i64,
    /// The type, if given.
    pub ty: Option<ValueType>,
}

impl Expr {
    /// Parse the text form `BASE[+OFF|-OFF…][:TYPE]`.
    pub fn parse(text: &str) -> RpcResult<Expr> {
        let bad = || {
            RpcError::bad_request(format!(
                "bad memory expression {text:?} (e.g. cfg+0x200:f32)"
            ))
        };
        let (body, ty) = match text.rsplit_once(':') {
            Some((body, ty)) => (
                body,
                Some(ValueType::parse(ty.trim()).ok_or_else(|| ValueType::unknown(ty.trim()))?),
            ),
            None => (text, None),
        };
        let body = body.trim();
        let split = body.find(['+', '-']).unwrap_or(body.len());
        let (base, mut rest) = body.split_at(split);
        let base = base.trim();
        let base = if base.starts_with(|c: char| c.is_ascii_digit()) {
            Base::Addr(parse_uint(base).ok_or_else(bad)?)
        } else if !base.is_empty()
            && base
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
        {
            Base::Symbol(base.to_owned())
        } else {
            return Err(bad());
        };
        let mut offset: i64 = 0;
        while !rest.is_empty() {
            let negative = rest.starts_with('-');
            rest = rest[1..].trim_start();
            let end = rest.find(['+', '-']).unwrap_or(rest.len());
            let term = parse_uint(&rest[..end])
                .and_then(|n| i64::try_from(n).ok())
                .ok_or_else(bad)?;
            offset = if negative {
                offset.checked_sub(term)
            } else {
                offset.checked_add(term)
            }
            .ok_or_else(bad)?;
            rest = &rest[end..];
        }
        Ok(Expr { base, offset, ty })
    }

    /// From a params object (`{symbol | addr, offset, as}`) or a string in
    /// the text form.
    pub fn from_value(value: &Value) -> RpcResult<Expr> {
        match value {
            Value::String(text) => Expr::parse(text),
            Value::Object(map) => Expr::from_params(map),
            _ => Err(RpcError::bad_request(
                "an expression is an object or a string",
            )),
        }
    }

    /// From `{symbol | addr, offset, as}`.
    pub fn from_params(map: &Map<String, Value>) -> RpcResult<Expr> {
        let base = match (map.get("symbol"), map.get("addr")) {
            (Some(Value::String(s)), None) => Base::Symbol(s.clone()),
            (None, Some(Value::String(a))) => Base::Addr(
                parse_uint(a).ok_or_else(|| RpcError::bad_request(format!("bad address {a:?}")))?,
            ),
            (None, Some(Value::Number(n))) => Base::Addr(
                n.as_u64()
                    .ok_or_else(|| RpcError::bad_request("bad address"))?,
            ),
            (Some(_), Some(_)) => {
                return Err(RpcError::bad_request("give addr or symbol, not both"))
            }
            _ => {
                return Err(RpcError::bad_request(
                    "addr (hex string) or symbol required",
                ))
            }
        };
        let offset = match map.get("offset") {
            None | Some(Value::Null) => 0,
            Some(v) => v
                .as_i64()
                .ok_or_else(|| RpcError::bad_request("offset must be an integer"))?,
        };
        let ty = match map.get("as") {
            None | Some(Value::Null) => None,
            Some(Value::String(name)) => {
                Some(ValueType::parse(name).ok_or_else(|| ValueType::unknown(name))?)
            }
            Some(_) => return Err(RpcError::bad_request("as must be a type name")),
        };
        Ok(Expr { base, offset, ty })
    }

    /// The absolute address, resolving a symbol with `lookup`.
    pub fn address(&self, lookup: impl Fn(&str) -> Option<u64>) -> RpcResult<u64> {
        let base = match &self.base {
            Base::Addr(a) => *a,
            Base::Symbol(name) => lookup(name).ok_or_else(|| {
                RpcError::bad_request(format!("unknown or unresolved symbol {name:?}"))
            })?,
        };
        base.checked_add_signed(self.offset)
            .ok_or_else(|| RpcError::bad_request("address overflows"))
    }
}

impl std::fmt::Display for Expr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.base {
            Base::Symbol(s) => f.write_str(s)?,
            Base::Addr(a) => write!(f, "{a:#x}")?,
        }
        if self.offset != 0 {
            let sign = if self.offset < 0 { '-' } else { '+' };
            write!(f, "{sign}{:#x}", self.offset.unsigned_abs())?;
        }
        if let Some(ty) = self.ty {
            write!(f, ":{}", ty.name())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn text_expressions_parse() {
        let e = Expr::parse("cfg+0x200:f32").unwrap();
        assert_eq!(
            (e.base, e.offset, e.ty),
            (Base::Symbol("cfg".into()), 0x200, Some(ValueType::F32))
        );
        let e = Expr::parse("0x1451c4480 + 8 - 0x10").unwrap();
        assert_eq!(
            (e.base, e.offset, e.ty),
            (Base::Addr(0x1451c4480), -8, None)
        );
        let e = Expr::parse("Camera:mat4").unwrap();
        assert_eq!(e.ty, Some(ValueType::Mat4));
        assert_eq!(
            Expr::parse("global.CAMERA").unwrap().base,
            Base::Symbol("global.CAMERA".into())
        );
        assert_eq!(
            Expr::parse("taa+0x1a8:u64").unwrap().to_string(),
            "taa+0x1a8:u64"
        );
        assert_eq!(Expr::parse("4096-1").unwrap().to_string(), "0x1000-0x1");
    }

    #[test]
    fn bad_text_is_refused() {
        for bad in [
            "",
            "+8",
            "cfg+",
            "cfg+zz",
            "cfg:f16",
            "c-fg!",
            "0xZZ",
            "cfg+99999999999999999999",
        ] {
            assert!(Expr::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn params_objects_parse() {
        let e = Expr::from_value(&json!({"symbol": "cfg", "offset": -4, "as": "u32"})).unwrap();
        assert_eq!(
            (e.base, e.offset, e.ty),
            (Base::Symbol("cfg".into()), -4, Some(ValueType::U32))
        );
        let e = Expr::from_value(&json!({"addr": "0x10", "offset": 0})).unwrap();
        assert_eq!(e.base, Base::Addr(16));
        assert!(Expr::from_value(&json!({"offset": 1})).is_err());
        assert!(Expr::from_value(&json!({"symbol": "a", "addr": "0x1"})).is_err());
        assert!(Expr::from_value(&json!({"symbol": "a", "as": "f16"})).is_err());
        assert!(Expr::from_value(&json!({"symbol": "a", "offset": "8"})).is_err());
        assert_eq!(
            Expr::from_value(&json!("dev+0x10da:u8")).unwrap().offset,
            0x10da
        );
    }

    #[test]
    fn resolution_adds_the_offset_and_checks_bounds() {
        let lookup = |s: &str| (s == "cfg").then_some(0x1000);
        assert_eq!(
            Expr::parse("cfg-0x10").unwrap().address(lookup).unwrap(),
            0xff0
        );
        assert!(Expr::parse("nope").unwrap().address(lookup).is_err());
        assert!(Expr::parse("0-1").unwrap().address(lookup).is_err());
    }

    #[test]
    fn values_round_trip_through_every_type() {
        let cases = [
            (ValueType::U8, json!([1, 255])),
            (ValueType::U16, json!(65535)),
            (ValueType::U32, json!([7])),
            (ValueType::U64, json!(u64::MAX)),
            (ValueType::F32, json!([0.5, -2.0])),
            (ValueType::F64, json!(1e300)),
        ];
        for (ty, value) in cases {
            let bytes = encode(&value, ty).unwrap();
            let expect = match &value {
                Value::Array(items) => items.clone(),
                v => vec![v.clone()],
            };
            assert_eq!(decode(&bytes, ty), expect, "{ty:?}");
        }
        let m: Vec<f64> = (0..16).map(f64::from).collect();
        let bytes = encode(&json!(m), ValueType::Mat4).unwrap();
        assert_eq!(bytes.len(), 64);
        assert_eq!(decode(&bytes, ValueType::Mat4), vec![json!(m)]);
        assert_eq!(encode(&json!([m, m]), ValueType::Mat4).unwrap().len(), 128);
        assert_eq!(
            encode(&json!("00ff 10"), ValueType::Bytes).unwrap(),
            [0, 255, 16]
        );
    }

    #[test]
    fn values_that_do_not_fit_are_refused() {
        assert!(encode(&json!(256), ValueType::U8).is_err());
        assert!(encode(&json!(-1), ValueType::U32).is_err());
        assert!(encode(&json!(1.5), ValueType::U32).is_err());
        assert_eq!(encode(&json!(2.0), ValueType::U16).unwrap(), [2, 0]);
        assert!(encode(&json!("1"), ValueType::F32).is_err());
        assert!(encode(&json!([]), ValueType::U8).is_err());
        assert!(encode(&json!(vec![1.0; 15]), ValueType::Mat4).is_err());
        assert!(encode(&json!("abc"), ValueType::Bytes).is_err());
        assert!(encode(&json!(""), ValueType::Bytes).is_err());
    }

    #[test]
    fn non_finite_floats_decode_to_null() {
        assert_eq!(
            decode(&f32::NAN.to_le_bytes(), ValueType::F32),
            vec![Value::Null]
        );
        assert_eq!(decode(&[1, 2, 3], ValueType::U16), vec![json!(0x0201)]);
    }
}
