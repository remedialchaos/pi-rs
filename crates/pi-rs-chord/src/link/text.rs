//! Positional debug codec. Raw byte segments follow one newline, not a base64 JSON envelope.
use super::{
    Body, BoundEntry, FailedMember, Message, Part, Payload, Result, StateEntry, WireEvent, invalid,
};
use crate::json::JsonValue;
use serde_json::{Value, json};
fn js(v: impl serde::Serialize) -> Result<String> {
    crate::json::stringify(&serde_json::to_value(v)?)
}
fn part(p: &Part) -> Result<String> {
    match p {
        Part::Deflated(b) => Ok(format!("[1,{}]", js(base64(b))?)),
        _ => Ok(format!(
            "[0,{}]",
            std::str::from_utf8(p.bytes()).map_err(|_| invalid("text: invalid UTF-8"))?
        )),
    }
}
fn tuple(fields: Vec<String>) -> String {
    format!("[{}]", fields.join(","))
}
fn failures(f: &[FailedMember]) -> Result<String> {
    let f = f
        .iter()
        .map(|f| Ok(json!([f.member, f.error.to_serde()?])))
        .collect::<Result<Vec<_>>>()?;
    js(f)
}
fn payload(fields: &mut Vec<String>, p: &Option<Payload>) -> Result<()> {
    if let Some(p) = p {
        fields.push(part(&p.part)?);
        if let Some(s) = &p.segments {
            fields.push(js(s.iter().map(Vec::len).collect::<Vec<_>>())?);
        }
    }
    Ok(())
}
pub fn describe_message(m: &Message) -> Result<String> {
    m.validate()?;
    let mut f = match &m.body {
        Body::Hello {
            protocol,
            incarnation,
            calls,
        } => {
            let mut f = vec![
                js("h")?,
                js(protocol)?,
                js(uuid::Uuid::from_bytes(*incarnation).to_string())?,
            ];
            if let Some(n) = calls {
                f.push(js(n)?);
            }
            f
        }
        Body::Hold {
            hold,
            type_name,
            version,
            fingerprint,
            params,
            address_kind,
            address_params,
            principal,
            incarnation,
            chain,
            limit,
        } => {
            let mut f = vec![
                js("H")?,
                js(hold)?,
                js(type_name)?,
                js(version)?,
                js(hex(fingerprint))?,
                part(params)?,
                js(json!([address_kind, address_params.to_serde()?]))?,
            ];
            let mut optional = vec![
                principal.as_ref().map(|p| p.to_serde()).transpose()?,
                incarnation.map(|id| json!(uuid::Uuid::from_bytes(id).to_string())),
                chain.as_ref().map(|ids| {
                    json!(
                        ids.iter()
                            .map(|id| uuid::Uuid::from_bytes(*id).to_string())
                            .collect::<Vec<_>>()
                    )
                }),
                limit.map(|v| json!(v)),
                m.trace.as_ref().map(|t| json!(t)),
            ];
            while optional.last().is_some_and(Option::is_none) {
                optional.pop();
            }
            for item in optional {
                f.push(js(item.unwrap_or(Value::Null))?);
            }
            f
        }
        Body::Release { hold } => vec![js("R")?, js(hold)?],
        Body::Call {
            call,
            hold,
            method,
            args,
        } => {
            let mut f = vec![js("c")?, js(call)?, js(hold)?, js(method)?];
            if m.trace.is_some() || m.old {
                f.push(
                    args.as_ref()
                        .map(|p| part(&p.part))
                        .transpose()?
                        .unwrap_or("null".into()),
                );
                f.push(js(args
                    .as_ref()
                    .and_then(|p| p.segments.as_ref())
                    .map(|s| s.iter().map(Vec::len).collect::<Vec<_>>()))?);
                f.push(js(&m.trace)?);
                if m.old {
                    f.push("true".into());
                }
            } else {
                payload(&mut f, args)?;
            }
            f
        }
        Body::Abort { call } => vec![js("x")?, js(call)?],
        Body::Bound(entries) => {
            let mut rows = Vec::new();
            for e in entries {
                let members = e
                    .members
                    .iter()
                    .map(|(n, p)| Ok(tuple(vec![js(n)?, part(p)?])))
                    .collect::<Result<Vec<_>>>()?;
                let mut fields = vec![js(e.hold)?, js(e.variant)?, tuple(members)];
                if !e.failed.is_empty() {
                    fields.push(failures(&e.failed)?);
                }
                rows.push(tuple(fields));
            }
            vec![js("b")?, tuple(rows)]
        }
        Body::State(entries) => {
            let mut rows = Vec::new();
            for e in entries {
                let members = e
                    .members
                    .iter()
                    .map(|(n, p)| Ok(tuple(vec![js(n)?, part(p)?])))
                    .collect::<Result<Vec<_>>>()?;
                let mut fields = vec![js(e.hold)?, tuple(members)];
                if !e.failed.is_empty() || !e.retired.is_empty() {
                    fields.push(failures(&e.failed)?);
                }
                if !e.retired.is_empty() {
                    fields.push(js(&e.retired)?);
                }
                rows.push(tuple(fields));
            }
            vec![js("s")?, tuple(rows)]
        }
        Body::Failed {
            hold,
            cause,
            error,
            retry_in,
        } => {
            let tag = ["t", "b", "w", "d"]
                .get(*cause as usize)
                .ok_or_else(|| invalid("text: unknown failure cause"))?;
            let mut f = vec![js("f")?, js(hold)?, js(tag)?, js(error.to_serde()?)?];
            if let Some(n) = retry_in {
                f.push(js(n)?);
            }
            f
        }
        Body::Result { call, value } => {
            let mut f = vec![js("r")?, js(call)?];
            payload(&mut f, value)?;
            f
        }
        Body::Error { call, error } => vec![js("e")?, js(call)?, js(error.to_serde()?)?],
        Body::Gone { hold, error } => vec![js("g")?, js(hold)?, js(error.to_serde()?)?],
        Body::Revoke(v) => vec![js("V")?, js(v.to_serde()?)?],
        Body::Retry { hold, hops } => vec![js("T")?, js(hold)?, js(hops)?],
        Body::Loading { hold } => vec![js("l")?, js(hold)?],
        Body::Ack { events } => {
            let mut f = vec![js("a")?];
            if let Some(n) = events {
                f.push(js(n)?);
            }
            f
        }
        Body::Event(events) => {
            let mut rows = Vec::new();
            for e in events {
                rows.push(tuple(vec![
                    js(e.hold)?,
                    js(e.member)?,
                    js(e.at)?,
                    part(&e.value)?,
                ]));
            }
            vec![js("E")?, tuple(rows)]
        }
        Body::Closed => vec![js("C")?],
        Body::Ping => vec![js("p")?],
        Body::Pong => vec![js("P")?],
    };
    let own = tuple(std::mem::take(&mut f));
    Ok(if let Some(origin) = m.origin {
        tuple(vec![js("@")?, js(origin)?, own])
    } else {
        own
    })
}
pub fn encode_text(m: &Message) -> Result<Vec<u8>> {
    let mut bytes = describe_message(m)?.into_bytes();
    let p = match &m.body {
        Body::Call { args, .. } => args.as_ref(),
        Body::Result { value, .. } => value.as_ref(),
        _ => None,
    };
    if let Some(segments) = p.and_then(|p| p.segments.as_ref()) {
        bytes.push(b'\n');
        for s in segments {
            bytes.extend(s);
        }
    }
    Ok(bytes)
}
fn list(v: &Value) -> Result<&[Value]> {
    v.as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| invalid("text: expected a tuple"))
}
fn field(f: &[Value], n: usize) -> Result<&Value> {
    f.get(n).ok_or_else(|| invalid("text: missing field"))
}
fn number(v: &Value) -> Result<u64> {
    v.as_u64()
        .filter(|v| *v <= super::MAX_SAFE_INTEGER)
        .ok_or_else(|| invalid("text: expected safe unsigned integer"))
}
fn string(v: &Value) -> Result<&str> {
    v.as_str().ok_or_else(|| invalid("text: expected string"))
}
fn arity(f: &[Value], allowed: &[usize]) -> Result<()> {
    if allowed.contains(&f.len()) {
        Ok(())
    } else {
        Err(invalid("text: wrong tuple arity"))
    }
}
fn uuid(v: &Value) -> Result<[u8; 16]> {
    let text = string(v)?;
    let id = uuid::Uuid::parse_str(text).map_err(|_| invalid("text: invalid incarnation"))?;
    if id.hyphenated().to_string() != text {
        return Err(invalid("text: invalid incarnation"));
    }
    Ok(*id.as_bytes())
}
fn error(v: &Value) -> JsonValue {
    JsonValue::from_serde(v.clone())
}
fn read_part(v: &Value) -> Result<Part> {
    let p = list(v)?;
    arity(p, &[2])?;
    match number(&p[0])? {
        0 => Ok(Part::Text(crate::json::stringify(&p[1])?)),
        1 => Ok(Part::Deflated(unbase64(string(&p[1])?)?)),
        _ => Err(invalid("text: unknown part codec")),
    }
}
fn read_payload(f: &[Value], rest: &[u8]) -> Result<Option<Payload>> {
    arity(f, &[0, 1, 2])?;
    if f.is_empty() {
        if !rest.is_empty() {
            return Err(invalid("text: byte segments without a payload"));
        }
        return Ok(None);
    }
    let p = read_part(&f[0])?;
    let segments = if f.len() == 2 {
        let mut offset = 0usize;
        let mut segments = Vec::new();
        for n in list(&f[1])? {
            let n = usize::try_from(number(n)?).map_err(|_| invalid("text: segment length"))?;
            let end = offset
                .checked_add(n)
                .ok_or_else(|| invalid("text: segment length overflow"))?;
            let s = rest
                .get(offset..end)
                .ok_or_else(|| invalid("text: truncated segments"))?;
            segments.push(s.to_vec());
            offset = end;
        }
        if offset != rest.len() {
            return Err(invalid("text: segments do not fill message"));
        }
        Some(segments)
    } else {
        if !rest.is_empty() {
            return Err(invalid("text: byte segments without lengths"));
        }
        None
    };
    Ok(Some(Payload { part: p, segments }))
}
fn pairs(v: &Value) -> Result<Vec<&[Value]>> {
    list(v)?
        .iter()
        .map(|v| {
            let p = list(v)?;
            arity(p, &[2])?;
            Ok(p)
        })
        .collect()
}
fn read_failures(v: Option<&Value>) -> Result<Vec<FailedMember>> {
    v.map(|v| {
        pairs(v)?
            .into_iter()
            .map(|p| {
                Ok(FailedMember {
                    member: number(&p[0])?,
                    error: error(&p[1]),
                })
            })
            .collect()
    })
    .unwrap_or_else(|| Ok(Vec::new()))
}
pub fn decode_text(bytes: &[u8]) -> Result<Message> {
    let newline = bytes.iter().position(|b| *b == b'\n');
    let (json, rest) = newline.map_or((bytes, &[][..]), |at| (&bytes[..at], &bytes[at + 1..]));
    let value: Value = serde_json::from_slice(json)?;
    let mut f = list(&value)?;
    let mut origin = None;
    if f.first().and_then(Value::as_str) == Some("@") {
        arity(f, &[3])?;
        origin = Some(number(&f[1])?);
        f = list(&f[2])?;
    }
    let tag = string(field(f, 0)?)?;
    let f = &f[1..];
    if tag != "c" && tag != "r" && !rest.is_empty() {
        return Err(invalid("text: segments on a non-payload message"));
    }
    let n = |i| number(field(f, i)?);
    let mut trace = None;
    let mut old = false;
    let body = match tag {
        "h" => {
            arity(f, &[2, 3])?;
            Body::Hello {
                protocol: n(0)?,
                incarnation: uuid(&f[1])?,
                calls: f.get(2).map(number).transpose()?,
            }
        }
        "H" => {
            arity(f, &[6, 7, 8, 9, 10, 11])?;
            let a = list(&f[5])?;
            arity(a, &[2])?;
            let optional = |i: usize| f.get(i).filter(|v| !v.is_null());
            let mut fp = [0; 8];
            let text = string(&f[3])?;
            if text.len() != 16
                || !text
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(invalid("text: fingerprint"));
            }
            for (i, b) in fp.iter_mut().enumerate() {
                *b = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16)
                    .map_err(|_| invalid("text: fingerprint"))?;
            }
            trace = optional(10)
                .map(|v| serde_json::from_value(v.clone()))
                .transpose()?;
            Body::Hold {
                hold: n(0)?,
                type_name: string(&f[1])?.into(),
                version: n(2)?,
                fingerprint: fp,
                params: read_part(&f[4])?,
                address_kind: string(&a[0])?.into(),
                address_params: Box::new(error(&a[1])),
                principal: optional(6).map(|v| Box::new(error(v))),
                incarnation: optional(7).map(uuid).transpose()?,
                chain: optional(8)
                    .map(|v| list(v)?.iter().map(uuid).collect::<Result<Vec<_>>>())
                    .transpose()?,
                limit: optional(9).map(number).transpose()?,
            }
        }
        "R" => {
            arity(f, &[1])?;
            Body::Release { hold: n(0)? }
        }
        "c" => {
            arity(f, &[3, 4, 5, 6, 7])?;
            let args = if f.len() < 6 {
                read_payload(&f[3..], rest)?
            } else {
                if !f[5].is_null() {
                    trace = Some(serde_json::from_value(f[5].clone())?);
                }
                if f.len() == 7 {
                    if f[6] != json!(true) {
                        return Err(invalid("text: old flag is true only"));
                    }
                    old = true;
                }
                let fields = if f[3].is_null() {
                    &f[3..3]
                } else if f[4].is_null() {
                    &f[3..4]
                } else {
                    &f[3..5]
                };
                read_payload(fields, rest)?
            };
            Body::Call {
                call: n(0)?,
                hold: n(1)?,
                method: n(2)?,
                args,
            }
        }
        "x" => {
            arity(f, &[1])?;
            Body::Abort { call: n(0)? }
        }
        "b" => {
            arity(f, &[1])?;
            let mut entries = Vec::new();
            for entry in list(&f[0])? {
                let e = list(entry)?;
                arity(e, &[3, 4])?;
                entries.push(BoundEntry {
                    hold: number(&e[0])?,
                    variant: number(&e[1])?,
                    members: pairs(&e[2])?
                        .into_iter()
                        .map(|p| Ok((number(&p[0])?, read_part(&p[1])?)))
                        .collect::<Result<_>>()?,
                    failed: read_failures(e.get(3))?,
                });
            }
            Body::Bound(entries)
        }
        "s" => {
            arity(f, &[1])?;
            let mut entries = Vec::new();
            for entry in list(&f[0])? {
                let e = list(entry)?;
                arity(e, &[2, 3, 4])?;
                entries.push(StateEntry {
                    hold: number(&e[0])?,
                    members: pairs(&e[1])?
                        .into_iter()
                        .map(|p| Ok((number(&p[0])?, read_part(&p[1])?)))
                        .collect::<Result<_>>()?,
                    failed: read_failures(e.get(2))?,
                    retired: e
                        .get(3)
                        .map(|v| list(v)?.iter().map(number).collect::<Result<Vec<_>>>())
                        .transpose()?
                        .unwrap_or_default(),
                });
            }
            Body::State(entries)
        }
        "f" => {
            arity(f, &[3, 4])?;
            let cause = match string(&f[1])? {
                "t" => 0,
                "b" => 1,
                "w" => 2,
                "d" => 3,
                _ => return Err(invalid("text: unknown failure cause")),
            };
            Body::Failed {
                hold: n(0)?,
                cause,
                error: error(&f[2]),
                retry_in: f.get(3).map(number).transpose()?,
            }
        }
        "r" => {
            arity(f, &[1, 2, 3])?;
            Body::Result {
                call: n(0)?,
                value: read_payload(&f[1..], rest)?,
            }
        }
        "e" => {
            arity(f, &[2])?;
            Body::Error {
                call: n(0)?,
                error: error(&f[1]),
            }
        }
        "g" => {
            arity(f, &[2])?;
            Body::Gone {
                hold: n(0)?,
                error: error(&f[1]),
            }
        }
        "V" => {
            arity(f, &[1])?;
            Body::Revoke(error(&f[0]))
        }
        "T" => {
            arity(f, &[2])?;
            Body::Retry {
                hold: n(0)?,
                hops: n(1)?,
            }
        }
        "l" => {
            arity(f, &[1])?;
            Body::Loading { hold: n(0)? }
        }
        "a" => {
            arity(f, &[0, 1])?;
            Body::Ack {
                events: f.first().map(number).transpose()?,
            }
        }
        "E" => {
            arity(f, &[1])?;
            let mut events = Vec::new();
            for entry in list(&f[0])? {
                let e = list(entry)?;
                arity(e, &[4])?;
                events.push(WireEvent {
                    hold: number(&e[0])?,
                    member: number(&e[1])?,
                    at: number(&e[2])?,
                    value: read_part(&e[3])?,
                });
            }
            Body::Event(events)
        }
        "C" => {
            arity(f, &[0])?;
            Body::Closed
        }
        "p" => {
            arity(f, &[0])?;
            Body::Ping
        }
        "P" => {
            arity(f, &[0])?;
            Body::Pong
        }
        _ => return Err(invalid("text: unknown kind")),
    };
    let m = Message {
        origin,
        trace,
        old,
        body,
    };
    m.validate()?;
    Ok(m)
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
fn base64(bytes: &[u8]) -> String {
    let mut text = String::new();
    for chunk in bytes.chunks(3) {
        let word = ((chunk[0] as u32) << 16)
            | ((chunk.get(1).copied().unwrap_or(0) as u32) << 8)
            | chunk.get(2).copied().unwrap_or(0) as u32;
        for shift in [18, 12, 6, 0] {
            text.push(BASE64[((word >> shift) & 63) as usize] as char);
        }
        if chunk.len() < 3 {
            for _ in chunk.len()..3 {
                text.pop();
            }
            for _ in chunk.len()..3 {
                text.push('=');
            }
        }
    }
    text
}
fn unbase64(text: &str) -> Result<Vec<u8>> {
    let text: String = text.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let raw = text.trim_end_matches('=');
    if raw.len() % 4 == 1
        || text.len() - raw.len() > 2
        || (text.contains('=') && !text.len().is_multiple_of(4))
    {
        return Err(invalid("text: invalid base64"));
    }
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for c in raw.bytes() {
        let n = BASE64
            .iter()
            .position(|v| *v == c)
            .ok_or_else(|| invalid("text: invalid base64"))?;
        acc = (acc << 6) | n as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}
