//! Chord binary messages and opaque raw-deflate parts. No JSON is opened while relaying a part.
use crate::{Error, Result, data::ChordError, json::JsonValue};
use flate2::{Compression, read::DeflateDecoder, write::DeflateEncoder};
use std::io::{Read, Write};
mod admission;
mod channel;
mod text;
mod transport;
mod values;
pub use channel::{Channel, ChannelHandler, ChannelOptions, Codec};
pub use text::{decode_text, describe_message, encode_text};
pub use transport::{
    IpcLink, IpcPort, Link, LinkEnd, WebSocketLink, WebSocketPort, memory_link_pair,
};
pub use values::{LinkValue, OriginPayload, parse_payload, payload_part, resolve_segments};

pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
pub const COMPRESS_ABOVE: usize = 16 * 1024;
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Part {
    Text(String),
    Bytes(Vec<u8>),
    Deflated(Vec<u8>),
}
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub part: usize,
    pub decoded: usize,
}
impl Limits {
    pub fn for_link(trusted: bool) -> Self {
        if trusted {
            Self {
                part: usize::MAX,
                decoded: usize::MAX,
            }
        } else {
            Self {
                part: 16 * 1024 * 1024,
                decoded: 64 * 1024 * 1024,
            }
        }
    }
}
fn invalid(s: &str) -> Error {
    Error::Invalid(s.into())
}
fn too_large(size: Option<usize>, limit: usize, measure: &str) -> Error {
    let what = size.map_or_else(|| "a part".into(), |size| format!("a part of {size} bytes"));
    ChordError {
        code: "too_large".into(),
        reason: Some("part".into()),
        message: format!("{what} is over the {measure} limit of {limit} bytes"),
        details: None,
    }
    .into()
}
impl Part {
    pub fn origin(text: String, compression: bool) -> Result<Self> {
        if !compression || text.encode_utf16().count() * 3 <= COMPRESS_ABOVE {
            return Ok(Self::Text(text));
        }
        let bytes = text.into_bytes();
        if bytes.len() <= COMPRESS_ABOVE {
            return Ok(Self::Bytes(bytes));
        }
        let mut compressor = DeflateEncoder::new(Vec::new(), Compression::fast());
        compressor.write_all(&bytes)?;
        let compressed = compressor.finish()?;
        Ok(if compressed.len() < bytes.len() {
            Self::Deflated(compressed)
        } else {
            Self::Bytes(bytes)
        })
    }
    pub fn bytes(&self) -> &[u8] {
        match self {
            Self::Text(s) => s.as_bytes(),
            Self::Bytes(b) | Self::Deflated(b) => b,
        }
    }
    pub fn open(&self, limits: Limits) -> Result<Vec<u8>> {
        if self.bytes().len() > limits.part {
            return Err(too_large(Some(self.bytes().len()), limits.part, "wire"));
        }
        if let Self::Deflated(bytes) = self {
            let mut reader = DeflateDecoder::new(bytes.as_slice());
            let mut output = Vec::new();
            reader
                .by_ref()
                .take((limits.decoded as u64).saturating_add(1))
                .read_to_end(&mut output)
                .map_err(|_| invalid("part: no raw deflate stream"))?;
            if output.len() > limits.decoded {
                return Err(too_large(None, limits.decoded, "decoded"));
            }
            Ok(output)
        } else {
            if self.bytes().len() > limits.decoded {
                return Err(too_large(
                    Some(self.bytes().len()),
                    limits.decoded,
                    "decoded",
                ));
            }
            Ok(self.bytes().to_vec())
        }
    }
    pub fn parse(&self, limits: Limits) -> Result<JsonValue> {
        let bytes = self.open(limits)?;
        let text = std::str::from_utf8(&bytes).map_err(|_| invalid("part: invalid UTF-8"))?;
        JsonValue::parse(text)
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct Payload {
    pub part: Part,
    pub segments: Option<Vec<Vec<u8>>>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct FailedMember {
    pub member: u64,
    pub error: JsonValue,
}
#[derive(Clone, Debug, PartialEq)]
pub struct BoundEntry {
    pub hold: u64,
    pub variant: u64,
    pub members: Vec<(u64, Part)>,
    pub failed: Vec<FailedMember>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct StateEntry {
    pub hold: u64,
    pub members: Vec<(u64, Part)>,
    pub failed: Vec<FailedMember>,
    pub retired: Vec<u64>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct WireEvent {
    pub hold: u64,
    pub member: u64,
    pub at: u64,
    pub value: Part,
}
#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    Hello {
        protocol: u64,
        incarnation: [u8; 16],
        calls: Option<u64>,
    },
    Hold {
        hold: u64,
        type_name: String,
        version: u64,
        fingerprint: [u8; 8],
        params: Part,
        address_kind: String,
        address_params: Box<JsonValue>,
        principal: Option<Box<JsonValue>>,
        incarnation: Option<[u8; 16]>,
        chain: Option<Vec<[u8; 16]>>,
        limit: Option<u64>,
    },
    Release {
        hold: u64,
    },
    Call {
        call: u64,
        hold: u64,
        method: u64,
        args: Option<Payload>,
    },
    Abort {
        call: u64,
    },
    Bound(Vec<BoundEntry>),
    State(Vec<StateEntry>),
    Failed {
        hold: u64,
        cause: u8,
        error: JsonValue,
        retry_in: Option<u64>,
    },
    Result {
        call: u64,
        value: Option<Payload>,
    },
    Error {
        call: u64,
        error: JsonValue,
    },
    Gone {
        hold: u64,
        error: JsonValue,
    },
    Retry {
        hold: u64,
        hops: u64,
    },
    Loading {
        hold: u64,
    },
    Ack {
        events: Option<u64>,
    },
    Closed,
    Ping,
    Pong,
    Revoke(JsonValue),
    Event(Vec<WireEvent>),
}
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub origin: Option<u64>,
    pub trace: Option<JsonValue>,
    pub old: bool,
    pub body: Body,
}
impl Message {
    pub fn new(body: Body) -> Self {
        Self {
            origin: None,
            trace: None,
            old: false,
            body,
        }
    }
    pub fn validate(&self) -> Result<()> {
        admission::check(self)?;
        if self.origin.is_some_and(|n| n == 0 || n > MAX_SAFE_INTEGER) {
            return Err(invalid("message: fails the message schema"));
        }
        if self.origin.is_some()
            && matches!(
                self.body,
                Body::Hello { .. } | Body::Ping | Body::Pong | Body::Revoke(_)
            )
        {
            return Err(invalid("message: origin on a kind without one"));
        }
        if self.old && !matches!(self.body, Body::Call { .. }) {
            return Err(invalid("message: old flag on a kind without one"));
        }
        if let Some(trace) = &self.trace {
            if !matches!(self.body, Body::Hold { .. } | Body::Call { .. }) {
                return Err(invalid("message: trace on a kind without one"));
            }
            let JsonValue::Object(map) = trace else {
                return Err(invalid("message: invalid trace carrier"));
            };
            if map.len() > 8 {
                return Err(invalid("message: invalid trace carrier"));
            }
            for (k, v) in map {
                let JsonValue::String(v) = v else {
                    return Err(invalid("message: invalid trace carrier"));
                };
                if k.0.is_empty()
                    || k.0.len() > 256
                    || k.0.iter().any(|n| !(0x21..=0x7e).contains(n))
                    || v.0.len() > 256
                    || v.0.iter().any(|n| !(0x20..=0x7e).contains(n))
                {
                    return Err(invalid("message: invalid trace carrier"));
                }
            }
        }
        match &self.body {
            Body::Hello { calls: Some(0), .. } | Body::Closed if self.origin.is_none() => {
                return Err(invalid("message: fails the message schema"));
            }
            Body::Hold {
                type_name,
                version,
                address_kind,
                chain,
                ..
            } if *version == 0
                || type_name.encode_utf16().count() > 256
                || address_kind.encode_utf16().count() > 256
                || chain.as_ref().is_some_and(|v| v.len() > 32) =>
            {
                return Err(invalid("message: fails the message schema"));
            }
            Body::Retry { hops, .. } if !(1..=16).contains(hops) => {
                return Err(invalid("message: fails the message schema"));
            }
            Body::Ack { events: Some(0) } => {
                return Err(invalid("message: fails the message schema"));
            }
            Body::Failed { cause, .. } if *cause >= 4 => {
                return Err(invalid("message: fails the message schema"));
            }
            _ => (),
        }
        Ok(())
    }
}
#[derive(Default)]
pub struct Writer {
    pub bytes: Vec<u8>,
}
impl Writer {
    pub fn byte(&mut self, n: u8) {
        self.bytes.push(n);
    }
    pub fn varint(&mut self, mut n: u64) -> Result<()> {
        if n > MAX_SAFE_INTEGER {
            return Err(invalid("binary: a varint does not fit"));
        }
        while n >= 128 {
            self.byte((n as u8 & 127) | 128);
            n >>= 7;
        }
        self.byte(n as u8);
        Ok(())
    }
    fn text(&mut self, s: &str, scale: u64) -> Result<()> {
        let length = s.len() as u64 * scale;
        if s.encode_utf16().count() >= 4096 {
            let mut rest = length;
            for i in 0..5 {
                self.byte((rest as u8 & 127) | if i < 4 { 128 } else { 0 });
                rest >>= 7;
            }
            if rest != 0 {
                return Err(invalid("binary: a string does not fit"));
            }
        } else {
            self.varint(length)?;
        }
        self.bytes.extend_from_slice(s.as_bytes());
        Ok(())
    }
    pub fn string(&mut self, s: &str) -> Result<()> {
        self.text(s, 1)
    }
    pub fn json(&mut self, v: &JsonValue) -> Result<()> {
        self.string(&v.to_json()?)
    }
    pub fn part(&mut self, p: &Part) -> Result<()> {
        match p {
            Part::Text(s) => self.text(s, 2),
            Part::Bytes(b) | Part::Deflated(b) => {
                self.varint(b.len() as u64 * 2 + u64::from(matches!(p, Part::Deflated(_))))?;
                self.bytes.extend(b);
                Ok(())
            }
        }
    }
    fn payload(&mut self, p: &Option<Payload>) -> Result<()> {
        let Some(p) = p else {
            return self.varint(0);
        };
        self.varint(if p.segments.is_some() { 3 } else { 1 })?;
        self.part(&p.part)?;
        if let Some(segments) = &p.segments {
            self.varint(segments.len() as u64)?;
            for b in segments {
                self.varint(b.len() as u64)?;
            }
            for b in segments {
                self.bytes.extend(b);
            }
        }
        Ok(())
    }
    fn failed_entries(&mut self, entries: &[FailedMember]) -> Result<()> {
        for entry in entries {
            self.varint(entry.member)?;
            self.json(&entry.error)?;
        }
        Ok(())
    }
}
pub struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }
    pub fn byte(&mut self) -> Result<u8> {
        let b = self
            .bytes
            .get(self.offset)
            .copied()
            .ok_or_else(|| invalid("binary: the message ends early"))?;
        self.offset += 1;
        Ok(b)
    }
    pub fn varint(&mut self) -> Result<u64> {
        let mut value = 0u64;
        for n in 0..8 {
            let byte = self.byte()?;
            value |= ((byte & 127) as u64) << (n * 7);
            if value > MAX_SAFE_INTEGER {
                return Err(invalid("binary: a varint does not fit"));
            }
            if byte < 128 {
                return Ok(value);
            }
        }
        Err(invalid("binary: a varint does not fit"))
    }
    pub fn raw(&mut self, length: usize) -> Result<&'a [u8]> {
        let remaining = &self.bytes[self.offset..];
        let out = remaining
            .get(..length)
            .ok_or_else(|| invalid("binary: the message ends early"))?;
        self.offset += length;
        Ok(out)
    }
    pub fn string(&mut self) -> Result<String> {
        let length = self.length()?;
        std::str::from_utf8(self.raw(length)?)
            .map(str::to_string)
            .map_err(|_| invalid("binary: invalid UTF-8"))
    }
    pub fn json(&mut self) -> Result<JsonValue> {
        JsonValue::parse(&self.string()?)
    }
    pub fn part(&mut self) -> Result<Part> {
        let prefix = self.varint()?;
        let n =
            usize::try_from(prefix / 2).map_err(|_| invalid("binary: the message ends early"))?;
        let b = self.raw(n)?.to_vec();
        Ok(if prefix & 1 != 0 {
            Part::Deflated(b)
        } else {
            Part::Bytes(b)
        })
    }
    fn length(&mut self) -> Result<usize> {
        usize::try_from(self.varint()?).map_err(|_| invalid("binary: a count does not fit"))
    }
    fn count(&mut self) -> Result<usize> {
        let count = self.length()?;
        if count > self.bytes.len() - self.offset {
            return Err(invalid("binary: a count does not fit"));
        }
        Ok(count)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.raw(N)?
            .try_into()
            .map_err(|_| invalid("binary: the message ends early"))
    }
    fn list<T>(&mut self, mut read: impl FnMut(&mut Self) -> Result<T>) -> Result<Vec<T>> {
        let count = self.count()?;
        (0..count).map(|_| read(self)).collect()
    }
    fn payload(&mut self) -> Result<Option<Payload>> {
        let flags = self.varint()?;
        if flags == 0 {
            return Ok(None);
        }
        if flags != 1 && flags != 3 {
            return Err(invalid("binary: unknown payload flags"));
        }
        let part = self.part()?;
        let segments = if flags == 3 {
            let lengths = self.list(Self::length)?;
            Some(
                lengths
                    .into_iter()
                    .map(|n| Ok(self.raw(n)?.to_vec()))
                    .collect::<Result<_>>()?,
            )
        } else {
            None
        };
        Ok(Some(Payload { part, segments }))
    }
    fn failed_entries(&mut self, count: usize) -> Result<Vec<FailedMember>> {
        if count > self.bytes.len() - self.offset {
            return Err(invalid("binary: a count does not fit"));
        }
        (0..count)
            .map(|_| {
                Ok(FailedMember {
                    member: self.varint()?,
                    error: self.json()?,
                })
            })
            .collect()
    }
    pub fn end(&self) -> Result<()> {
        if self.offset != self.bytes.len() {
            Err(invalid("binary: bytes after the message"))
        } else {
            Ok(())
        }
    }
}
pub fn encode(message: &Message) -> Result<Vec<u8>> {
    message.validate()?;
    let mut w = Writer::default();
    let kind = match message.body {
        Body::Hello { .. } => 0,
        Body::Hold { .. } => 1,
        Body::Release { .. } => 2,
        Body::Call { .. } => 3,
        Body::Abort { .. } => 4,
        Body::Bound(_) => 5,
        Body::State(_) => 6,
        Body::Failed { .. } => 7,
        Body::Result { .. } => 8,
        Body::Error { .. } => 9,
        Body::Gone { .. } => 10,
        Body::Retry { .. } => 11,
        Body::Loading { .. } => 12,
        Body::Ack { .. } => 13,
        Body::Closed => 14,
        Body::Ping => 15,
        Body::Pong => 16,
        Body::Revoke(_) => 17,
        Body::Event(_) => 18,
    };
    w.byte(
        kind | if message.origin.is_some() { 0x80 } else { 0 }
            | if message.trace.is_some() { 0x40 } else { 0 }
            | if message.old || matches!(message.body, Body::Ack { events: Some(_) }) {
                0x20
            } else {
                0
            },
    );
    if let Some(origin) = message.origin {
        w.varint(origin)?;
    }
    if let Some(trace) = &message.trace {
        w.json(trace)?;
    }
    match &message.body {
        Body::Hello {
            protocol,
            incarnation,
            calls,
        } => {
            w.varint(*protocol)?;
            w.bytes.extend(incarnation);
            w.varint(calls.unwrap_or(0))?;
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
            w.varint(*hold)?;
            w.string(type_name)?;
            w.varint(*version)?;
            w.bytes.extend(fingerprint);
            w.part(params)?;
            w.string(address_kind)?;
            w.json(address_params)?;
            w.byte(
                u8::from(principal.is_some())
                    | if incarnation.is_some() { 2 } else { 0 }
                    | if chain.is_some() { 4 } else { 0 }
                    | if limit.is_some() { 8 } else { 0 },
            );
            if let Some(p) = principal {
                w.json(p)?;
            }
            if let Some(i) = incarnation {
                w.bytes.extend(i);
            }
            if let Some(c) = chain {
                w.varint(c.len() as u64)?;
                for i in c {
                    w.bytes.extend(i);
                }
            }
            if let Some(l) = limit {
                w.varint(*l)?;
            }
        }
        Body::Release { hold } | Body::Loading { hold } => w.varint(*hold)?,
        Body::Retry { hold, hops } => {
            w.varint(*hold)?;
            w.varint(*hops)?;
        }
        Body::Call {
            call,
            hold,
            method,
            args,
        } => {
            w.varint(*call)?;
            w.varint(*hold)?;
            w.varint(*method)?;
            w.payload(args)?;
        }
        Body::Abort { call } => w.varint(*call)?,
        Body::Bound(holds) => {
            w.varint(holds.len() as u64)?;
            for h in holds {
                w.varint(h.hold)?;
                w.varint(h.variant)?;
                w.varint(h.members.len() as u64)?;
                for (m, p) in &h.members {
                    w.varint(*m)?;
                    w.part(p)?;
                }
                w.varint(h.failed.len() as u64)?;
                w.failed_entries(&h.failed)?;
            }
        }
        Body::State(holds) => {
            w.varint(holds.len() as u64)?;
            for h in holds {
                w.varint(h.hold)?;
                w.varint(h.members.len() as u64)?;
                for (m, p) in &h.members {
                    w.varint(*m)?;
                    w.part(p)?;
                }
                w.varint(h.failed.len() as u64 * 2 + u64::from(!h.retired.is_empty()))?;
                w.failed_entries(&h.failed)?;
                if !h.retired.is_empty() {
                    w.varint(h.retired.len() as u64)?;
                    for m in &h.retired {
                        w.varint(*m)?;
                    }
                }
            }
        }
        Body::Failed {
            hold,
            cause,
            error,
            retry_in,
        } => {
            w.varint(*hold)?;
            w.byte(*cause);
            w.json(error)?;
            w.varint(retry_in.map_or(0, |r| r.saturating_add(1)))?;
        }
        Body::Result { call, value } => {
            w.varint(*call)?;
            w.payload(value)?;
        }
        Body::Error { call, error } => {
            w.varint(*call)?;
            w.json(error)?;
        }
        Body::Gone { hold, error } => {
            w.varint(*hold)?;
            w.json(error)?;
        }
        Body::Revoke(v) => w.json(v)?,
        Body::Event(events) => {
            w.varint(events.len() as u64)?;
            for e in events {
                w.varint(e.hold)?;
                w.varint(e.member)?;
                w.varint(e.at)?;
                w.part(&e.value)?;
            }
        }
        Body::Ack { events: Some(n) } => w.varint(*n)?,
        Body::Ack { events: None } | Body::Closed | Body::Ping | Body::Pong => (),
    }
    Ok(w.bytes)
}
pub fn decode(bytes: &[u8]) -> Result<Message> {
    let mut r = Reader::new(bytes);
    let first = r.byte()?;
    let kind = first & 0x1f;
    if kind > 18 || (first & 0x20 != 0 && kind != 3 && kind != 13) {
        return Err(invalid("binary: an unknown kind"));
    }
    let origin = if first & 0x80 != 0 {
        Some(r.varint()?)
    } else {
        None
    };
    let trace = if first & 0x40 != 0 {
        Some(r.json()?)
    } else {
        None
    };
    let body = match kind {
        0 => {
            let protocol = r.varint()?;
            let incarnation = r.array()?;
            let calls = r.varint()?;
            Body::Hello {
                protocol,
                incarnation,
                calls: (calls != 0).then_some(calls),
            }
        }
        1 => {
            let hold = r.varint()?;
            let type_name = r.string()?;
            let version = r.varint()?;
            let fingerprint = r.array()?;
            let params = r.part()?;
            let address_kind = r.string()?;
            let address_params = Box::new(r.json()?);
            let flags = r.byte()?;
            if flags > 15 {
                return Err(invalid("binary: unknown hold flags"));
            }
            let principal = if flags & 1 != 0 {
                Some(Box::new(r.json()?))
            } else {
                None
            };
            let incarnation = if flags & 2 != 0 {
                Some(r.array()?)
            } else {
                None
            };
            let chain = if flags & 4 != 0 {
                Some(r.list(Reader::array)?)
            } else {
                None
            };
            let limit = if flags & 8 != 0 {
                Some(r.varint()?)
            } else {
                None
            };
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
            }
        }
        2 => Body::Release { hold: r.varint()? },
        3 => Body::Call {
            call: r.varint()?,
            hold: r.varint()?,
            method: r.varint()?,
            args: r.payload()?,
        },
        4 => Body::Abort { call: r.varint()? },
        5 => Body::Bound(r.list(|r| {
            let hold = r.varint()?;
            let variant = r.varint()?;
            let members = r.list(|r| Ok((r.varint()?, r.part()?)))?;
            let count = r.count()?;
            let failed = r.failed_entries(count)?;
            Ok(BoundEntry {
                hold,
                variant,
                members,
                failed,
            })
        })?),
        6 => Body::State(r.list(|r| {
            let hold = r.varint()?;
            let members = r.list(|r| Ok((r.varint()?, r.part()?)))?;
            let flags = r.varint()?;
            let failed = r.failed_entries(
                usize::try_from(flags / 2).map_err(|_| invalid("binary: a count does not fit"))?,
            )?;
            let retired = if flags & 1 != 0 {
                r.list(Reader::varint)?
            } else {
                Vec::new()
            };
            Ok(StateEntry {
                hold,
                members,
                failed,
                retired,
            })
        })?),
        7 => {
            let hold = r.varint()?;
            let cause = r.byte()?;
            if cause >= 4 {
                return Err(invalid("binary: an unknown failure kind"));
            }
            let error = r.json()?;
            let retry = r.varint()?;
            Body::Failed {
                hold,
                cause,
                error,
                retry_in: retry.checked_sub(1),
            }
        }
        8 => Body::Result {
            call: r.varint()?,
            value: r.payload()?,
        },
        9 => Body::Error {
            call: r.varint()?,
            error: r.json()?,
        },
        10 => Body::Gone {
            hold: r.varint()?,
            error: r.json()?,
        },
        11 => Body::Retry {
            hold: r.varint()?,
            hops: r.varint()?,
        },
        12 => Body::Loading { hold: r.varint()? },
        13 => Body::Ack {
            events: if first & 0x20 != 0 {
                Some(r.varint()?)
            } else {
                None
            },
        },
        14 => Body::Closed,
        15 => Body::Ping,
        16 => Body::Pong,
        17 => Body::Revoke(r.json()?),
        18 => Body::Event(r.list(|r| {
            Ok(WireEvent {
                hold: r.varint()?,
                member: r.varint()?,
                at: r.varint()?,
                value: r.part()?,
            })
        })?),
        _ => return Err(invalid("binary: an unknown kind")),
    };
    r.end()?;
    let message = Message {
        origin,
        trace,
        old: kind == 3 && first & 0x20 != 0,
        body,
    };
    message.validate()?;
    Ok(message)
}
