//! Persistent, alias-safe revisions. Only ancestors of an edited value are copied.
use super::*;
use indexmap::IndexMap;
use std::sync::{Arc, Weak};

#[derive(Clone, Debug)]
pub struct Revision(pub(super) Arc<Node>);
#[derive(Clone)]
pub struct WeakRevision(Weak<Node>);
impl WeakRevision {
    pub fn upgrade(&self) -> Option<Revision> {
        self.0.upgrade().map(Revision)
    }
}
#[derive(Clone, Debug)]
pub(super) enum Node {
    Scalar(JsonValue),
    Array(Vec<Revision>),
    Object(IndexMap<JsonString, Revision>),
}
impl Revision {
    pub fn downgrade(&self) -> WeakRevision {
        WeakRevision(Arc::downgrade(&self.0))
    }
    pub(crate) fn identity(&self) -> usize {
        Arc::as_ptr(&self.0) as usize
    }
    pub fn is_container(&self) -> bool {
        !matches!(self.0.as_ref(), Node::Scalar(_))
    }
    pub fn elements(&self) -> Option<&[Revision]> {
        match self.0.as_ref() {
            Node::Array(a) => Some(a),
            _ => None,
        }
    }
    pub fn array(elements: Vec<Revision>) -> Self {
        Self(Arc::new(Node::Array(elements)))
    }
    pub fn from_json(value: JsonValue) -> DeltaResult<Self> {
        let node = match value {
            JsonValue::Array(a) => Node::Array(
                a.into_iter()
                    .map(Self::from_json)
                    .collect::<DeltaResult<_>>()?,
            ),
            JsonValue::Object(o) => Node::Object(
                o.into_iter()
                    .map(|(k, v)| Ok((k, Self::from_json(v)?)))
                    .collect::<DeltaResult<_>>()?,
            ),
            JsonValue::Number(n) if !n.is_finite() => {
                return Err(shape("revision contains a non-finite number"));
            }
            JsonValue::Number(n) => Node::Scalar(JsonValue::Number(if n == 0.0 { 0.0 } else { n })),
            v => Node::Scalar(v),
        };
        Ok(Self(Arc::new(node)))
    }
    pub fn to_json(&self) -> JsonValue {
        match self.0.as_ref() {
            Node::Scalar(v) => v.clone(),
            Node::Array(a) => JsonValue::Array(a.iter().map(Self::to_json).collect()),
            Node::Object(o) => JsonValue::Object(
                super::diff::ordered_keys(o)
                    .into_iter()
                    .map(|k| (k.clone(), o[k].to_json()))
                    .collect(),
            ),
        }
    }
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub fn at(&self, path: &[Segment]) -> DeltaResult<&Self> {
        let mut node = self;
        for s in path {
            node = match (node.0.as_ref(), s) {
                (Node::Object(o), s) => o.get(&object_key(s)),
                (Node::Array(a), Segment::Index(i)) => a.get(*i),
                _ => None,
            }
            .ok_or_else(|| path_error(path))?;
        }
        Ok(node)
    }
    fn at_mut(&mut self, path: &[Segment]) -> DeltaResult<&mut Self> {
        let Some((head, tail)) = path.split_first() else {
            return Ok(self);
        };
        let child = match (Arc::make_mut(&mut self.0), head) {
            (Node::Object(o), s) => o.get_mut(&object_key(s)),
            (Node::Array(a), Segment::Index(i)) => a.get_mut(*i),
            _ => None,
        }
        .ok_or_else(|| path_error(path))?;
        child.at_mut(tail)
    }
    pub(super) fn same(&self, other: &Self) -> bool {
        if self.ptr_eq(other) {
            return true;
        }
        matches!((self.0.as_ref(), other.0.as_ref()), (Node::Scalar(a),Node::Scalar(b)) if a==b)
    }
    pub(super) fn equal(&self, other: &Self) -> bool {
        if self.same(other) {
            return true;
        }
        match (self.0.as_ref(), other.0.as_ref()) {
            (Node::Array(a), Node::Array(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.equal(b))
            }
            (Node::Object(a), Node::Object(b)) => {
                a.len() == b.len() && a.iter().all(|(k, v)| b.get(k).is_some_and(|b| v.equal(b)))
            }
            _ => false,
        }
    }
}
pub fn apply_shared(root: &Revision, ops: &[Op]) -> DeltaResult<Revision> {
    apply_inner(root, ops, true)
}
pub(super) fn apply_draft(root: &Revision, ops: &[Op]) -> DeltaResult<Revision> {
    apply_inner(root, ops, false)
}
fn apply_inner(root: &Revision, ops: &[Op], safe: bool) -> DeltaResult<Revision> {
    let mut result = root.clone();
    for op in ops {
        if safe {
            op.validate()?
        } else {
            if let Some(path) = op.path()
                && path.is_empty()
                && !matches!(op, Op::Splice(..) | Op::Permute(..))
            {
                return Err(shape("path is empty"));
            }
            if let Op::Permute(_, order) = op {
                Op::Permute(Vec::new(), order.clone()).validate()?;
            }
        }
        if let Op::Replace(v) = op {
            result = Revision::from_json(v.clone())?;
            continue;
        }
        let path = op.path().ok_or_else(|| shape("missing path"))?;
        match op {
            Op::Splice(_, index, remove, items) => {
                let items = items
                    .iter()
                    .cloned()
                    .map(Revision::from_json)
                    .collect::<DeltaResult<Vec<_>>>()?;
                let Node::Array(a) = Arc::make_mut(&mut result.at_mut(path)?.0) else {
                    return Err(path_error(path));
                };
                let at = (*index).min(a.len());
                let end = at + (*remove).min(a.len() - at);
                a.splice(at..end, items);
            }
            Op::Permute(_, order) => {
                let Node::Array(a) = Arc::make_mut(&mut result.at_mut(path)?.0) else {
                    return Err(path_error(path));
                };
                if a.len() != order.len() {
                    return Err(path_error(path));
                }
                let before = a.clone();
                *a = order.iter().map(|i| before[*i].clone()).collect();
            }
            Op::Set(..) | Op::Delete(_) => {
                let replacement = if let Op::Set(_, v) = op {
                    Some(Revision::from_json(v.clone())?)
                } else {
                    None
                };
                let (key, parents) = path.split_last().ok_or_else(|| shape("empty path"))?;
                let parent = result.at_mut(parents)?;
                match (Arc::make_mut(&mut parent.0), key, replacement) {
                    (Node::Object(o), key, Some(v)) => {
                        o.insert(object_key(key), v);
                    }
                    (Node::Object(o), key, None) => {
                        o.shift_remove(&object_key(key));
                    }
                    (Node::Array(a), Segment::Index(i), Some(v)) if *i <= a.len() => {
                        if *i == a.len() { a.push(v) } else { a[*i] = v }
                    }
                    (Node::Array(a), Segment::Index(i), None) if *i < a.len() => {
                        a.remove(*i);
                    }
                    _ => return Err(path_error(path)),
                }
            }
            Op::Append(_, text) => {
                let Node::Scalar(JsonValue::String(s)) = Arc::make_mut(&mut result.at_mut(path)?.0)
                else {
                    return Err(path_error(path));
                };
                s.0.extend(&text.0);
            }
            Op::Truncate(_, count) => {
                let Node::Scalar(JsonValue::String(s)) = Arc::make_mut(&mut result.at_mut(path)?.0)
                else {
                    return Err(path_error(path));
                };
                s.0.drain(..(*count).min(s.0.len()));
            }
            Op::Replace(_) => return Err(shape("unexpected replacement")),
        }
    }
    Ok(result)
}

/// Weak cache entries cannot validate a different node after allocator address reuse.
#[derive(Default)]
pub struct JsonRevisionValidator {
    cached: HashMap<usize, Weak<Node>>,
}
impl JsonRevisionValidator {
    pub fn validate(&mut self, value: &Revision) -> DeltaResult<()> {
        self.cached.retain(|_, weak| weak.strong_count() > 0);
        self.visit(value)
    }
    fn visit(&mut self, value: &Revision) -> DeltaResult<()> {
        let id = Arc::as_ptr(&value.0) as usize;
        if self
            .cached
            .get(&id)
            .and_then(Weak::upgrade)
            .is_some_and(|v| Arc::ptr_eq(&v, &value.0))
        {
            return Ok(());
        }
        match value.0.as_ref() {
            Node::Scalar(JsonValue::Number(n)) if !n.is_finite() => {
                return Err(shape("revision contains a non-finite number"));
            }
            Node::Array(a) => {
                for v in a {
                    self.visit(v)?
                }
            }
            Node::Object(o) => {
                for v in o.values() {
                    self.visit(v)?
                }
            }
            _ => (),
        }
        self.cached.insert(id, Arc::downgrade(&value.0));
        Ok(())
    }
}
