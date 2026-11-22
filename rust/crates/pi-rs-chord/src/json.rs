//! Strict JSON with JavaScript UTF-16 strings, including unpaired surrogates.
//! Rust's owned tree cannot contain sparse arrays, accessors, aliases or cycles.
use crate::{Error, Result};
use indexmap::IndexMap;
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{Error as DeError, Visitor},
};
use serde_json::{Value, value::RawValue};
use std::fmt;

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JsonString(pub Vec<u16>);
impl From<&str> for JsonString {
    fn from(s: &str) -> Self {
        Self(s.encode_utf16().collect())
    }
}
impl From<String> for JsonString {
    fn from(s: String) -> Self {
        Self::from(s.as_str())
    }
}
impl JsonString {
    pub fn to_utf8(&self) -> Result<String> {
        String::from_utf16(&self.0)
            .map_err(|_| Error::Invalid("string contains an unpaired UTF-16 surrogate".into()))
    }
    pub fn to_json(&self) -> String {
        let mut out = String::from("\"");
        for decoded in char::decode_utf16(self.0.iter().copied()) {
            match decoded {
                Ok('"') => out.push_str("\\\""),
                Ok('\\') => out.push_str("\\\\"),
                Ok('\n') => out.push_str("\\n"),
                Ok('\r') => out.push_str("\\r"),
                Ok('\t') => out.push_str("\\t"),
                Ok('\u{8}') => out.push_str("\\b"),
                Ok('\u{c}') => out.push_str("\\f"),
                Ok(c) if c < ' ' => out.push_str(&format!("\\u{:04x}", c as u32)),
                Ok(c) => out.push(c),
                Err(e) => out.push_str(&format!("\\u{:04x}", e.unpaired_surrogate())),
            }
        }
        out.push('"');
        out
    }
}
impl<'de> Deserialize<'de> for JsonString {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct Bytes;
        impl Visitor<'_> for Bytes {
            type Value = JsonString;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON string")
            }
            fn visit_bytes<E: DeError>(self, bytes: &[u8]) -> std::result::Result<JsonString, E> {
                let mut units = Vec::new();
                let mut i = 0;
                while i < bytes.len() {
                    let first = bytes[i];
                    let (n, mut cp) = if first < 0x80 {
                        (1, first as u32)
                    } else if first < 0xe0 {
                        (2, (first & 31) as u32)
                    } else if first < 0xf0 {
                        (3, (first & 15) as u32)
                    } else {
                        (4, (first & 7) as u32)
                    };
                    let rest = bytes
                        .get(i + 1..i + n)
                        .ok_or_else(|| E::custom("truncated WTF-8 string"))?;
                    for byte in rest {
                        cp = (cp << 6) | (byte & 63) as u32;
                    }
                    if cp <= 0xffff {
                        units.push(cp as u16);
                    } else {
                        let cp = cp - 0x10000;
                        units.push(0xd800 | (cp >> 10) as u16);
                        units.push(0xdc00 | (cp & 1023) as u16);
                    }
                    i += n;
                }
                Ok(JsonString(units))
            }
        }
        d.deserialize_bytes(Bytes)
    }
}
impl Serialize for JsonString {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let raw = RawValue::from_string(self.to_json()).map_err(serde::ser::Error::custom)?;
        raw.serialize(s)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Number(f64),
    String(JsonString),
    Array(Vec<JsonValue>),
    Object(IndexMap<JsonString, JsonValue>),
}
impl JsonValue {
    pub fn parse(text: &str) -> Result<Self> {
        Ok(serde_json::from_str(text)?)
    }
    pub fn from_serde(value: Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Bool(b) => Self::Bool(b),
            Value::Number(n) => Self::Number(n.as_f64().unwrap_or(0.0)),
            Value::String(s) => Self::String(s.into()),
            Value::Array(a) => Self::Array(a.into_iter().map(Self::from_serde).collect()),
            Value::Object(o) => Self::Object(
                o.into_iter()
                    .map(|(k, v)| (k.into(), Self::from_serde(v)))
                    .collect(),
            ),
        }
    }
    pub fn to_serde(&self) -> Result<Value> {
        Ok(serde_json::from_str(&self.to_json()?)?)
    }
    pub fn to_json(&self) -> Result<String> {
        Ok(match self {
            Self::Null => "null".into(),
            Self::Bool(b) => b.to_string(),
            Self::Number(n) => {
                if !n.is_finite() {
                    return Err(Error::Invalid(
                        "Value contains a non-finite number and is not strict JSON".into(),
                    ));
                }
                number_to_string(*n)
            }
            Self::String(s) => s.to_json(),
            Self::Array(a) => format!(
                "[{}]",
                a.iter()
                    .map(Self::to_json)
                    .collect::<Result<Vec<_>>>()?
                    .join(",")
            ),
            Self::Object(o) => {
                // JSON.stringify enumerates array-index names numerically before other keys.
                let mut entries: Vec<_> = o.iter().collect();
                entries.sort_by_key(|(k, _)| {
                    k.to_utf8()
                        .ok()
                        .and_then(|k| {
                            k.parse::<u32>()
                                .ok()
                                .filter(|n| *n != u32::MAX && n.to_string() == k)
                        })
                        .map_or((1, 0), |n| (0, n))
                });
                format!(
                    "{{{}}}",
                    entries
                        .into_iter()
                        .map(|(k, v)| Ok(format!("{}:{}", k.to_json(), v.to_json()?)))
                        .collect::<Result<Vec<_>>>()?
                        .join(",")
                )
            }
        })
    }
    pub fn get(&self, key: &str) -> Option<&Self> {
        match self {
            Self::Object(o) => o.get(&JsonString::from(key)),
            _ => None,
        }
    }
    pub fn is_primitive(&self) -> bool {
        !matches!(self, Self::Array(_) | Self::Object(_))
    }
}
impl<'de> Deserialize<'de> for JsonValue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let raw = Box::<RawValue>::deserialize(d)?;
        let text = raw.get();
        let value = match text.as_bytes().first() {
            Some(b'"') => serde_json::from_str::<JsonString>(text).map(Self::String),
            Some(b'[') => serde_json::from_str::<Vec<Self>>(text).map(Self::Array),
            Some(b'{') => {
                serde_json::from_str::<IndexMap<JsonString, Self>>(text).map(Self::Object)
            }
            Some(b'n') => Ok(Self::Null),
            Some(b't') => Ok(Self::Bool(true)),
            Some(b'f') => Ok(Self::Bool(false)),
            _ => serde_json::from_str::<f64>(text).map(Self::Number),
        };
        value.map_err(D::Error::custom)
    }
}
impl Serialize for JsonValue {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let raw = RawValue::from_string(self.to_json().map_err(serde::ser::Error::custom)?)
            .map_err(serde::ser::Error::custom)?;
        raw.serialize(s)
    }
}
impl From<Value> for JsonValue {
    fn from(v: Value) -> Self {
        Self::from_serde(v)
    }
}

pub fn copy_json(value: &JsonValue) -> Result<JsonValue> {
    value.to_json()?;
    Ok(value.clone())
}
pub fn is_json_value(value: &JsonValue) -> bool {
    value.to_json().is_ok()
}

/// The ECMAScript finite-number spelling used by JSON.stringify and identity keys.
pub fn number_to_string(x: f64) -> String {
    if x == 0.0 {
        return "0".into();
    }
    if !x.is_finite() {
        return x.to_string();
    }
    if x.fract() == 0.0 && x.abs() < 9_007_199_254_740_992.0 {
        return (x as i64).to_string();
    }
    let json = x.to_string();
    let (sign, text) = json
        .strip_prefix('-')
        .map_or(("", json.as_str()), |rest| ("-", rest));
    let (mantissa, exponent) = text.find(['e', 'E']).map_or((text, 0), |i| {
        (&text[..i], text[i + 1..].parse::<i64>().unwrap_or(0))
    });
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let all = format!("{int}{frac}");
    let lead = all.len() - all.trim_start_matches('0').len();
    let digits = all[lead..].trim_end_matches('0');
    let k = digits.len() as i64;
    let n = int.len() as i64 - lead as i64 + exponent;
    let mut out = sign.to_string();
    if k <= n && n <= 21 {
        out.push_str(digits);
        out.extend(std::iter::repeat_n('0', (n - k) as usize));
    } else if 0 < n && n <= 21 {
        out.push_str(&digits[..n as usize]);
        out.push('.');
        out.push_str(&digits[n as usize..]);
    } else if -6 < n && n <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (-n) as usize));
        out.push_str(digits);
    } else {
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let e = n - 1;
        out.push('e');
        if e >= 0 {
            out.push('+');
        }
        out.push_str(&e.to_string());
    }
    out
}

pub fn stringify(value: &Value) -> Result<String> {
    JsonValue::from_serde(value.clone()).to_json()
}
