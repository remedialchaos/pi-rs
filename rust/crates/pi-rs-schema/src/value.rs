// The value model a schema checks: JSON, plus the three TypeBox extensions that never pass through JSON
// (`bigint`, `bytes` and `undefined`).
//
// Numbers are `f64`, as in JavaScript: JSON has one number type, and the upstream package compares and
// coerces numbers with JavaScript semantics (`-0 == 0`, `NaN != NaN`, integers beyond 2^53 lose precision).

use indexmap::IndexMap;
use num_bigint::BigInt;
use std::cmp::Ordering;

/// An object's keys, in the order they were written.
pub type Map = IndexMap<String, Value>;

/// A value a schema checks.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// A number, finite or not.
    Number(f64),
    /// A bigint (TypeBox's `Type.BigInt`): an integer of any size.
    BigInt(BigInt),
    /// A string.
    String(String),
    /// `undefined` (TypeBox's `Type.Undefined`).
    Undefined,
    /// A byte string (TypeBox's `Type.Bytes`).
    Bytes(Vec<u8>),
    /// An array.
    Array(Vec<Value>),
    /// An object.
    Object(Map),
}

impl Value {
    /// Whether the value is `null`.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Whether the value is a number or a bigint.
    pub fn is_numeric(&self) -> bool {
        matches!(self, Value::Number(_) | Value::BigInt(_))
    }

    /// The value as a string, if it is one.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    /// The value as an array, if it is one.
    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    /// The value as an object, if it is one.
    pub fn as_object(&self) -> Option<&Map> {
        match self {
            Value::Object(map) => Some(map),
            _ => None,
        }
    }

    /// The value as a number, if it is one.
    pub fn as_number(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// The value as a bigint, if it is one.
    pub fn as_bigint(&self) -> Option<&BigInt> {
        match self {
            Value::BigInt(b) => Some(b),
            _ => None,
        }
    }
}

impl From<serde_json::Value> for Value {
    fn from(value: serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Bool(b),
            serde_json::Value::Number(n) => Value::Number(n.as_f64().unwrap_or(f64::NAN)),
            serde_json::Value::String(s) => Value::String(s),
            serde_json::Value::Array(items) => {
                Value::Array(items.into_iter().map(Value::from).collect())
            }
            serde_json::Value::Object(map) => {
                Value::Object(map.into_iter().map(|(k, v)| (k, Value::from(v))).collect())
            }
        }
    }
}

impl From<&serde_json::Value> for Value {
    fn from(value: &serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Bool(*b),
            serde_json::Value::Number(n) => Value::Number(n.as_f64().unwrap_or(f64::NAN)),
            serde_json::Value::String(s) => Value::String(s.clone()),
            serde_json::Value::Array(items) => {
                Value::Array(items.iter().map(Value::from).collect())
            }
            serde_json::Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), Value::from(v)))
                    .collect(),
            ),
        }
    }
}

impl TryFrom<Value> for serde_json::Value {
    type Error = ConversionError;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        match value {
            Value::Null => Ok(serde_json::Value::Null),
            Value::Bool(b) => Ok(serde_json::Value::Bool(b)),
            Value::Number(n) => serde_json::Number::from_f64(n)
                .map(serde_json::Value::Number)
                .ok_or(ConversionError::NotFinite),
            Value::BigInt(b) => Ok(serde_json::Value::String(b.to_string())),
            Value::String(s) => Ok(serde_json::Value::String(s)),
            Value::Undefined => Err(ConversionError::Undefined),
            Value::Bytes(bytes) => Ok(serde_json::Value::Array(
                bytes.into_iter().map(serde_json::Value::from).collect(),
            )),
            Value::Array(items) => items
                .into_iter()
                .map(serde_json::Value::try_from)
                .collect::<Result<Vec<_>, _>>()
                .map(serde_json::Value::Array),
            Value::Object(map) => map
                .into_iter()
                .map(|(k, v)| Ok((k, serde_json::Value::try_from(v)?)))
                .collect::<Result<serde_json::Map<String, serde_json::Value>, ConversionError>>()
                .map(serde_json::Value::Object),
        }
    }
}

/// Why a value has no JSON form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConversionError {
    /// A number that JSON cannot hold (`NaN`, an infinity).
    NotFinite,
    /// `undefined`, which JSON has no form of.
    Undefined,
}

impl std::fmt::Display for ConversionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConversionError::NotFinite => f.write_str("the number is not finite"),
            ConversionError::Undefined => f.write_str("undefined has no JSON form"),
        }
    }
}

impl std::error::Error for ConversionError {}

/// The JSON equality of `const`, `enum` and `uniqueItems`: the same primitives, arrays with equal items in
/// order, objects with the same keys and equal values (key order does not matter).
///
/// Rust's derived equality already has these semantics: `f64` equality gives `-0 == 0` and `NaN != NaN`, as
/// JavaScript's `===` does, and a map compares by keys, not by order.
pub fn equal(a: &Value, b: &Value) -> bool {
    a == b
}

/// The number of Unicode code points of `s`: JSON Schema string lengths count code points, not UTF-16 units.
///
/// A Rust string cannot hold an unpaired surrogate, so every code point is one `char`.
pub fn code_points(s: &str) -> usize {
    s.chars().count()
}

/// Compares a bigint with a number, as JavaScript does: the mathematical value of both.
///
/// `None` when the number is `NaN`, which compares with nothing.
pub fn cmp_bigint_number(big: &BigInt, number: f64) -> Option<Ordering> {
    if number.is_nan() {
        return None;
    }
    if number == f64::INFINITY {
        return Some(Ordering::Less);
    }
    if number == f64::NEG_INFINITY {
        return Some(Ordering::Greater);
    }
    let floor = number.floor();
    let floor_big = bigint_of_f64(floor)?;
    match big.cmp(&floor_big) {
        Ordering::Less => Some(Ordering::Less),
        Ordering::Greater => Some(Ordering::Greater),
        Ordering::Equal => {
            if number == floor {
                Some(Ordering::Equal)
            } else {
                Some(Ordering::Less)
            }
        }
    }
}

/// The exact integer value of a finite `f64`.
pub fn bigint_of_f64(value: f64) -> Option<BigInt> {
    if !value.is_finite() {
        return None;
    }
    let truncated = value.trunc();
    // `format!` writes the exact decimal value of an integral f64, so the parse is exact.
    let text = format!("{truncated:.0}");
    text.parse::<BigInt>().ok()
}

/// The decimal form of a number, as (digits, exponent): the value is `digits * 10^exponent`.
///
/// JavaScript's `Number::toString` writes an exponent for large and small magnitudes; Rust's `LowerExp` writes
/// one always, with the same shortest round-trip digits. The pair is the same value either way.
pub fn decimal(value: f64) -> (BigInt, i64) {
    let text = format!("{value:e}");
    let (mantissa, exponent) = match text.split_once('e') {
        Some((mantissa, exponent)) => (mantissa, exponent),
        None => (text.as_str(), "0"),
    };
    let exponent: i64 = exponent.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let count = digits.len() as i64;
    let digits = if digits.is_empty() {
        BigInt::from(0)
    } else {
        digits.parse().unwrap_or_else(|_| BigInt::from(0))
    };
    // The exponent of the last digit: the value is `digits * 10^exponent`.
    (digits, exponent - (count - 1))
}

/// The decimal form of a number, as JavaScript writes it: the shortest digits that round-trip, in plain notation
/// for the magnitudes JavaScript writes plainly and in exponential notation for the others.
pub fn js_number_to_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        };
    }
    if value == 0.0 {
        return "0".to_string();
    }
    let negative = value < 0.0;
    let magnitude = value.abs();
    // `{:e}` writes the shortest round-tripping digits: "1.5e2" for 150.
    let text = format!("{magnitude:e}");
    let (mantissa, exponent) = match text.split_once('e') {
        Some((mantissa, exponent)) => (mantissa, exponent),
        None => (text.as_str(), "0"),
    };
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let count = digits.len() as i32;
    // `n` is the position of the decimal point, `k` the number of digits.
    let n = exponent + 1;
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if k_in_range(n, count) {
        if count <= n {
            out.push_str(digits);
            for _ in 0..(n - count) {
                out.push('0');
            }
        } else {
            out.push_str(&digits[..n as usize]);
            out.push('.');
            out.push_str(&digits[n as usize..]);
        }
    } else if n > 0 && n <= 21 {
        out.push_str(&digits[..n as usize]);
        out.push('.');
        out.push_str(&digits[n as usize..]);
    } else if n > -6 && n <= 0 {
        out.push_str("0.");
        for _ in 0..(-n) {
            out.push('0');
        }
        out.push_str(digits);
    } else {
        out.push_str(&digits[..1]);
        if count > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        if n > 0 {
            out.push('+');
        } else {
            out.push('-');
        }
        out.push_str(&(n - 1).abs().to_string());
    }
    out
}

/// Whether the decimal point of a number with `n` before it and `count` digits is written plainly.
fn k_in_range(n: i32, count: i32) -> bool {
    count <= n && n <= 21
}
