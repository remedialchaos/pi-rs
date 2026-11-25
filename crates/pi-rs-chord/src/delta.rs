//! Decoded revision operations and the stateful, batch-scoped wire dictionary.
mod diff;
mod draft_ops;
mod revision;
mod tracker;
use crate::json::{JsonString, JsonValue};
pub use diff::{diff_revisions, diff_shared, overlap};
pub use revision::{JsonRevisionValidator, Revision, WeakRevision, apply_shared};
use std::collections::{HashMap, HashSet};
pub use tracker::{Change, Prepared, PreparedData, Tracker, draft};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Segment {
    Key(JsonString),
    Index(usize),
}
pub type Path = Vec<Segment>;
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum DeltaError {
    #[error("unsafe path segment: {0}")]
    UnsafePath(String),
    #[error("unresolvable path: {0}")]
    Path(String),
    #[error("{0}")]
    Shape(String),
}
pub type DeltaResult<T> = std::result::Result<T, DeltaError>;
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Replace(JsonValue),
    Set(Path, JsonValue),
    Delete(Path),
    Append(Path, JsonString),
    Truncate(Path, usize),
    Splice(Path, usize, usize, Vec<JsonValue>),
    Permute(Path, Vec<usize>),
}
fn shape(s: &str) -> DeltaError {
    DeltaError::Shape(s.into())
}
fn path_error(path: &[Segment]) -> DeltaError {
    DeltaError::Path(format!("{path:?}"))
}
fn number(v: &JsonValue) -> DeltaResult<usize> {
    match v {
        JsonValue::Number(n)
            if n.is_finite()
                && *n >= 0.0
                && n.fract() == 0.0
                && *n <= 9_007_199_254_740_991.0
                && *n < usize::MAX as f64 =>
        {
            Ok(*n as usize)
        }
        _ => Err(shape("expected a nonnegative integer")),
    }
}
fn num(n: usize) -> JsonValue {
    JsonValue::Number(n as f64)
}
fn text(s: &str) -> JsonValue {
    JsonValue::String(s.into())
}
fn reserved(key: &JsonString) -> bool {
    ["__proto__", "constructor", "prototype"]
        .iter()
        .any(|s| key == &JsonString::from(*s))
}
pub fn assert_safe_path(path: &[Segment]) -> DeltaResult<()> {
    for s in path {
        if let Segment::Key(key) = s
            && reserved(key)
        {
            return Err(DeltaError::UnsafePath(String::from_utf16_lossy(&key.0)));
        }
    }
    Ok(())
}
fn path_value(path: &[Segment]) -> JsonValue {
    JsonValue::Array(
        path.iter()
            .map(|s| match s {
                Segment::Key(s) => JsonValue::String(s.clone()),
                Segment::Index(n) => num(*n),
            })
            .collect(),
    )
}
fn parse_path(value: &JsonValue) -> DeltaResult<Path> {
    let JsonValue::Array(a) = value else {
        return Err(shape("path is not an array"));
    };
    let path = a
        .iter()
        .map(|s| match s {
            JsonValue::String(s) => Ok(Segment::Key(s.clone())),
            _ => number(s).map(Segment::Index),
        })
        .collect::<DeltaResult<Path>>()?;
    assert_safe_path(&path)?;
    Ok(path)
}
impl Op {
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Replace(_) => None,
            Self::Set(p, _)
            | Self::Delete(p)
            | Self::Append(p, _)
            | Self::Truncate(p, _)
            | Self::Splice(p, _, _, _)
            | Self::Permute(p, _) => Some(p),
        }
    }
    pub fn validate(&self) -> DeltaResult<()> {
        if let Some(p) = self.path() {
            assert_safe_path(p)?;
            if p.is_empty() && !matches!(self, Self::Splice(..) | Self::Permute(..)) {
                return Err(shape("path is empty"));
            }
        }
        if let Self::Permute(_, order) = self {
            let mut seen = vec![false; order.len()];
            for i in order {
                if *i >= seen.len() || seen[*i] {
                    return Err(shape("m permutation is not a bijection"));
                }
                seen[*i] = true;
            }
        }
        Ok(())
    }
    pub fn to_json(&self) -> JsonValue {
        JsonValue::Array(match self {
            Self::Replace(v) => vec![text("r"), v.clone()],
            Self::Set(p, v) => vec![text("s"), path_value(p), v.clone()],
            Self::Delete(p) => vec![text("d"), path_value(p)],
            Self::Append(p, s) => vec![text("a"), path_value(p), JsonValue::String(s.clone())],
            Self::Truncate(p, n) => vec![text("t"), path_value(p), num(*n)],
            Self::Splice(p, i, r, v) => vec![
                text("p"),
                path_value(p),
                num(*i),
                num(*r),
                JsonValue::Array(v.clone()),
            ],
            Self::Permute(p, v) => vec![
                text("m"),
                path_value(p),
                JsonValue::Array(v.iter().map(|i| num(*i)).collect()),
            ],
        })
    }
    pub fn from_json(value: &JsonValue) -> DeltaResult<Self> {
        let JsonValue::Array(a) = value else {
            return Err(shape("op is not a tuple"));
        };
        let verb = match a.first() {
            Some(JsonValue::String(s)) => s.to_utf8().map_err(|_| shape("unknown op verb"))?,
            _ => return Err(shape("unknown op verb")),
        };
        let arity = match verb.as_str() {
            "r" | "d" => 2,
            "s" | "a" | "t" | "m" => 3,
            "p" => 5,
            _ => return Err(shape("unknown op verb")),
        };
        if a.len() != arity {
            return Err(shape(&format!("{verb} arity")));
        }
        if verb == "r" {
            return Ok(Self::Replace(a[1].clone()));
        }
        let path = parse_path(&a[1])?;
        let op = match verb.as_str() {
            "s" => Self::Set(path, a[2].clone()),
            "d" => Self::Delete(path),
            "a" => match &a[2] {
                JsonValue::String(s) => Self::Append(path, s.clone()),
                _ => return Err(shape("a shape")),
            },
            "t" => Self::Truncate(path, number(&a[2])?),
            "p" => match &a[4] {
                JsonValue::Array(v) => {
                    Self::Splice(path, number(&a[2])?, number(&a[3])?, v.clone())
                }
                _ => return Err(shape("p items")),
            },
            "m" => match &a[2] {
                JsonValue::Array(v) => {
                    Self::Permute(path, v.iter().map(number).collect::<DeltaResult<_>>()?)
                }
                _ => return Err(shape("m permutation is not an array")),
            },
            _ => return Err(shape("unknown op verb")),
        };
        op.validate()?;
        Ok(op)
    }
}
fn object_key(s: &Segment) -> JsonString {
    match s {
        Segment::Key(k) => k.clone(),
        Segment::Index(i) => i.to_string().into(),
    }
}
fn resolve_mut<'a>(
    mut node: &'a mut JsonValue,
    path: &[Segment],
) -> DeltaResult<&'a mut JsonValue> {
    for s in path {
        node = match (node, s) {
            (JsonValue::Object(o), s) => {
                o.get_mut(&object_key(s)).ok_or_else(|| path_error(path))?
            }
            (JsonValue::Array(a), Segment::Index(i)) => {
                a.get_mut(*i).ok_or_else(|| path_error(path))?
            }
            (JsonValue::Array(_), Segment::Key(k)) => {
                return Err(DeltaError::UnsafePath(String::from_utf16_lossy(&k.0)));
            }
            _ => return Err(path_error(path)),
        };
    }
    Ok(node)
}
/// The caller owns the batch and the mutable root. No other revision is mutated.
pub fn apply(mut root: JsonValue, ops: &[Op]) -> DeltaResult<JsonValue> {
    for op in ops {
        op.validate()?;
        if let Op::Replace(v) = op {
            root = v.clone();
            continue;
        }
        let path = op.path().ok_or_else(|| shape("missing operation path"))?;
        if let Op::Splice(_, i, r, items) = op {
            let JsonValue::Array(a) = resolve_mut(&mut root, path)? else {
                return Err(path_error(path));
            };
            let start = (*i).min(a.len());
            let end = start + r.min(&(a.len() - start));
            a.splice(start..end, items.clone());
            continue;
        }
        if let Op::Permute(_, order) = op {
            let JsonValue::Array(a) = resolve_mut(&mut root, path)? else {
                return Err(path_error(path));
            };
            if order.len() != a.len() {
                return Err(path_error(path));
            }
            let previous = a.clone();
            *a = order.iter().map(|i| previous[*i].clone()).collect();
            continue;
        }
        let (key, parent_path) = path.split_last().ok_or_else(|| shape("path is empty"))?;
        let parent = resolve_mut(&mut root, parent_path)?;
        if let JsonValue::Array(a) = parent {
            match key {
                Segment::Index(i) if *i <= a.len() => (),
                _ => return Err(DeltaError::UnsafePath(format!("{key:?}"))),
            }
        }
        match op {
            Op::Set(_, v) => match parent {
                JsonValue::Object(o) => {
                    o.insert(object_key(key), v.clone());
                }
                JsonValue::Array(a) => {
                    if let Segment::Index(i) = key {
                        if *i == a.len() {
                            a.push(v.clone());
                        } else {
                            a[*i] = v.clone();
                        }
                    }
                }
                _ => return Err(path_error(path)),
            },
            Op::Delete(_) => match parent {
                JsonValue::Object(o) => {
                    o.shift_remove(&object_key(key));
                }
                JsonValue::Array(a) => {
                    if let Segment::Index(i) = key {
                        if *i >= a.len() {
                            return Err(path_error(path));
                        }
                        a.remove(*i);
                    }
                }
                _ => return Err(path_error(path)),
            },
            Op::Append(..) | Op::Truncate(..) => {
                let JsonValue::String(current) = resolve_mut(parent, std::slice::from_ref(key))?
                else {
                    return Err(path_error(path));
                };
                if let Op::Truncate(_, count) = op {
                    current.0.drain(..(*count).min(current.0.len()));
                } else if let Op::Append(_, s) = op {
                    current.0.extend(&s.0);
                }
            }
            _ => return Err(shape("unexpected operation")),
        }
    }
    Ok(root)
}
pub fn apply_immutable(root: &JsonValue, ops: &[Op]) -> DeltaResult<JsonValue> {
    apply(root.clone(), ops)
}

#[derive(Default)]
pub struct Encoder {
    seen: HashSet<Path>,
    ids: HashMap<Path, usize>,
    next: usize,
}
impl Encoder {
    pub fn encode(&mut self, ops: &[Op]) -> DeltaResult<Vec<JsonValue>> {
        let mut previous: Option<&Path> = None;
        let mut out = Vec::new();
        for op in ops {
            op.validate()?;
            let JsonValue::Array(mut tuple) = op.to_json() else {
                return Err(shape("operation is not a tuple"));
            };
            let Some(path) = op.path() else {
                self.seen.clear();
                self.ids.clear();
                self.next = 0;
                previous = None;
                out.push(JsonValue::Array(tuple));
                continue;
            };
            if previous == Some(path) {
                tuple.remove(1);
            } else {
                if let Some(id) = self.ids.get(path) {
                    tuple[1] = num(*id);
                } else if self.seen.contains(path) {
                    let id = self.next;
                    self.next += 1;
                    self.ids.insert(path.clone(), id);
                    out.push(JsonValue::Array(vec![text("#"), num(id), path_value(path)]));
                    tuple[1] = num(id);
                } else {
                    self.seen.insert(path.clone());
                }
                previous = Some(path);
            }
            out.push(JsonValue::Array(tuple));
        }
        Ok(out)
    }
}
#[derive(Default)]
pub struct Decoder {
    paths: HashMap<usize, Path>,
}
impl Decoder {
    pub fn decode(&mut self, wire: &[JsonValue]) -> DeltaResult<Vec<Op>> {
        let mut previous: Option<Path> = None;
        let mut out = Vec::new();
        for value in wire {
            let JsonValue::Array(a) = value else {
                return Err(shape("op is not a tuple"));
            };
            let verb = match a.first() {
                Some(JsonValue::String(s)) => s.to_utf8().map_err(|_| shape("unknown op verb"))?,
                _ => return Err(shape("unknown op verb")),
            };
            if verb == "#" {
                if a.len() != 3 {
                    return Err(shape("# shape"));
                }
                self.paths.insert(number(&a[1])?, parse_path(&a[2])?);
                continue;
            }
            if verb == "r" {
                let op = Op::from_json(value)?;
                self.paths.clear();
                previous = None;
                out.push(op);
                continue;
            }
            let arity = match verb.as_str() {
                "d" => 2,
                "s" | "a" | "t" | "m" => 3,
                "p" => 5,
                _ => return Err(shape("unknown op verb")),
            };
            let mut full = a.clone();
            if a.len() == arity - 1 {
                let path = previous.as_ref().ok_or_else(|| path_error(&[]))?;
                full.insert(1, path_value(path));
            } else if a.len() == arity {
                let path = match &a[1] {
                    JsonValue::Number(_) => self
                        .paths
                        .get(&number(&a[1])?)
                        .cloned()
                        .ok_or_else(|| DeltaError::Path(format!("{:?}", a[1])))?,
                    _ => parse_path(&a[1])?,
                };
                previous = Some(path.clone());
                full[1] = path_value(&path);
            } else {
                return Err(shape(&format!("{verb} arity")));
            }
            out.push(Op::from_json(&JsonValue::Array(full))?);
        }
        Ok(out)
    }
}
pub const MAX_PENDING_OPS: usize = 10_000;
pub const MAX_DELTA_OPERATIONS: usize = 4_096;
#[derive(Clone, Debug, PartialEq)]
pub enum PendingTake {
    Ops(Vec<Op>),
    Replace,
}
#[derive(Default)]
pub struct PendingOps {
    ops: Vec<Op>,
    batch: bool,
    overflow: bool,
    leaves: HashMap<Path, usize>,
}
impl PendingOps {
    pub fn size(&self) -> usize {
        if self.overflow { 1 } else { self.ops.len() }
    }
    pub fn take(&mut self) -> PendingTake {
        let old = std::mem::take(self);
        if old.overflow {
            PendingTake::Replace
        } else {
            PendingTake::Ops(old.ops)
        }
    }
    pub fn push(&mut self, ops: &[Op]) {
        if ops.is_empty() {
            return;
        }
        if matches!(ops.first(), Some(Op::Replace(_))) {
            self.ops = ops.to_vec();
            self.batch = true;
            self.overflow = false;
            self.leaves.clear();
            return;
        }
        if self.overflow {
            return;
        }
        if self.ops.is_empty() {
            self.ops = ops.to_vec();
            self.batch = true;
            return;
        }
        if self.batch {
            let old = std::mem::take(&mut self.ops);
            self.batch = false;
            self.fold(&old);
        }
        if !self.overflow {
            self.fold(ops);
        }
    }
    fn fold(&mut self, ops: &[Op]) {
        for op in ops {
            if matches!(op, Op::Replace(_)) {
                self.ops.clear();
                self.leaves.clear();
            }
            let leaf =
                matches!(op, Op::Append(..)) || matches!(op,Op::Set(_,v) if v.is_primitive());
            if leaf {
                if let Some(path) = op.path() {
                    if let Some(at) = self.leaves.get(path).copied() {
                        let merged = match (&self.ops[at], op) {
                            (_, Op::Set(..)) => Some(op.clone()),
                            (Op::Append(p, a), Op::Append(_, b)) => {
                                let mut s = a.clone();
                                s.0.extend(&b.0);
                                Some(Op::Append(p.clone(), s))
                            }
                            (Op::Set(p, JsonValue::String(a)), Op::Append(_, b)) => {
                                let mut s = a.clone();
                                s.0.extend(&b.0);
                                Some(Op::Set(p.clone(), JsonValue::String(s)))
                            }
                            _ => None,
                        };
                        if let Some(merged) = merged {
                            self.ops[at] = merged;
                            continue;
                        }
                    }
                    self.leaves.insert(path.clone(), self.ops.len());
                }
            } else {
                self.leaves.clear();
            }
            self.ops.push(op.clone());
            if self.ops.len() > MAX_PENDING_OPS {
                self.overflow = true;
                self.ops.clear();
                self.leaves.clear();
                return;
            }
        }
    }
}
