//! Deterministic upstream-compatible revision differencing.
use super::{revision::Node, *};
use std::{collections::VecDeque, sync::Arc};

#[derive(Hash, Eq, PartialEq)]
enum Identity {
    Null,
    Bool(bool),
    Number(u64),
    Text(JsonString),
    Container(usize),
}
fn identity(v: &Revision) -> Identity {
    match v.0.as_ref() {
        Node::Scalar(JsonValue::Null) => Identity::Null,
        Node::Scalar(JsonValue::Bool(b)) => Identity::Bool(*b),
        Node::Scalar(JsonValue::Number(n)) => {
            Identity::Number(if *n == 0.0 { 0 } else { n.to_bits() })
        }
        Node::Scalar(JsonValue::String(s)) => Identity::Text(s.clone()),
        _ => Identity::Container(Arc::as_ptr(&v.0) as usize),
    }
}
fn permutation(a: &[Revision], b: &[Revision]) -> Option<Vec<usize>> {
    let mut positions: HashMap<Identity, VecDeque<usize>> = HashMap::new();
    for (i, v) in a.iter().enumerate() {
        positions.entry(identity(v)).or_default().push_back(i);
    }
    b.iter()
        .map(|v| positions.get_mut(&identity(v))?.pop_front())
        .collect()
}
pub fn overlap(a: &JsonString, b: &JsonString, scan: usize) -> usize {
    if a.0.is_empty() || b.0.is_empty() || scan == 0 {
        return 0;
    }
    let tail = &a.0[a.0.len().saturating_sub(scan)..];
    for h in [64.min(b.0.len()), 1] {
        let mut tried = 0;
        for k in 0..=tail.len().saturating_sub(h) {
            if tail.len() < h || tail[k..k + h] != b.0[..h] {
                continue;
            }
            tried += 1;
            if tried > 8 {
                break;
            }
            let n = tail.len() - k;
            if n <= b.0.len() && tail[k..] == b.0[..n] {
                return n;
            }
        }
        if h == 1 {
            break;
        }
    }
    0
}
struct Differ {
    ops: Vec<Op>,
    overflow: bool,
}
impl Differ {
    fn emit(&mut self, op: Op) {
        if self.ops.len() >= MAX_DELTA_OPERATIONS {
            self.overflow = true
        } else if !self.overflow {
            self.ops.push(op)
        }
    }
    fn set(&mut self, path: Path, v: &Revision) {
        self.emit(if path.is_empty() {
            Op::Replace(v.to_json())
        } else {
            Op::Set(path, v.to_json())
        })
    }
    fn value(&mut self, a: &Revision, b: &Revision, path: Path) {
        if a.same(b) || self.overflow {
            return;
        }
        match (a.0.as_ref(), b.0.as_ref()) {
            (Node::Scalar(JsonValue::String(a)), Node::Scalar(JsonValue::String(b)))
                if !path.is_empty() =>
            {
                if b.0.len() > a.0.len() && b.0.starts_with(&a.0) {
                    self.emit(Op::Append(path, JsonString(b.0[a.0.len()..].to_vec())));
                    return;
                }
                let shared = overlap(a, b, 65_536);
                if shared == 0 {
                    self.emit(Op::Set(path, JsonValue::String(b.clone())))
                } else {
                    self.emit(Op::Truncate(path.clone(), a.0.len() - shared));
                    if b.0.len() > shared {
                        self.emit(Op::Append(path, JsonString(b.0[shared..].to_vec())))
                    }
                }
            }
            (Node::Array(a), Node::Array(b)) => {
                if a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.equal(b)) {
                    return;
                }
                if a.len() == b.len()
                    && a.len() > 1
                    && !a[0].equal(&b[0])
                    && !a[a.len() - 1].equal(&b[b.len() - 1])
                    && let Some(order) = permutation(a, b)
                {
                    self.emit(Op::Permute(path, order));
                    return;
                }
                self.region(a, b, &path, 0);
            }
            (Node::Object(a), Node::Object(b)) => {
                if a.keys().chain(b.keys()).any(reserved) {
                    if !a.iter().all(|(k, v)| b.get(k).is_some_and(|b| v.equal(b)))
                        || a.len() != b.len()
                    {
                        self.set(path, &Revision(Arc::new(Node::Object(b.clone()))));
                    }
                    return;
                }
                for (key, value) in ordered_keys(b)
                    .into_iter()
                    .filter_map(|key| b.get_key_value(key))
                {
                    if self.overflow {
                        return;
                    }
                    let mut p = path.clone();
                    p.push(Segment::Key(key.clone()));
                    if let Some(before) = a.get(key) {
                        self.value(before, value, p)
                    } else {
                        self.set(p, value)
                    }
                }
                for key in ordered_keys(a) {
                    if self.overflow {
                        return;
                    }
                    if !b.contains_key(key) {
                        let mut p = path.clone();
                        p.push(Segment::Key(key.clone()));
                        self.emit(Op::Delete(p))
                    }
                }
            }
            _ => self.set(path, b),
        }
    }
    fn region(&mut self, mut a: &[Revision], mut b: &[Revision], path: &Path, mut output: usize) {
        if self.overflow {
            return;
        }
        while !a.is_empty() && !b.is_empty() && a[0].equal(&b[0]) {
            a = &a[1..];
            b = &b[1..];
            output += 1
        }
        while !a.is_empty() && !b.is_empty() && a[a.len() - 1].equal(&b[b.len() - 1]) {
            a = &a[..a.len() - 1];
            b = &b[..b.len() - 1]
        }
        if a.is_empty() && b.is_empty() {
            return;
        }
        if a.is_empty() || b.is_empty() {
            self.splice(a, b, path, output);
            return;
        }
        if a.len() == b.len() {
            let positional: Vec<_> = a
                .iter()
                .zip(b)
                .enumerate()
                .filter_map(|(i, (a, b))| a.equal(b).then_some((i, i)))
                .collect();
            if !positional.is_empty() {
                self.matches(a, b, path, output, &positional);
                return;
            }
        }
        if let Some(matches) = subsequence(a, b)
            && !matches.is_empty()
        {
            self.matches(a, b, path, output, &matches);
            return;
        }
        let matches = anchors(a, b);
        if !matches.is_empty() {
            self.matches(a, b, path, output, &matches);
            return;
        }
        let matches = semantic(a, b);
        if !matches.is_empty() {
            self.matches(a, b, path, output, &matches);
            return;
        }
        if a.len() == 1 && b.len() == 1 {
            let mut p = path.clone();
            p.push(Segment::Index(output));
            self.value(&a[0], &b[0], p)
        } else {
            self.splice(a, b, path, output)
        }
    }
    fn splice(&mut self, a: &[Revision], b: &[Revision], path: &Path, output: usize) {
        self.emit(Op::Splice(
            path.clone(),
            output,
            a.len(),
            b.iter().map(Revision::to_json).collect(),
        ))
    }
    fn matches(
        &mut self,
        a: &[Revision],
        b: &[Revision],
        path: &Path,
        mut output: usize,
        matches: &[(usize, usize)],
    ) {
        let (mut left, mut right) = (0, 0);
        for &(i, j) in matches {
            if self.overflow {
                return;
            }
            self.region(&a[left..i], &b[right..j], path, output);
            output += j - right;
            if !a[i].equal(&b[j]) {
                let mut p = path.clone();
                p.push(Segment::Index(output));
                self.value(&a[i], &b[j], p)
            }
            output += 1;
            left = i + 1;
            right = j + 1;
        }
        self.region(&a[left..], &b[right..], path, output);
    }
}
// JS enumerates canonical array-index keys before other string keys.
pub(super) fn ordered_keys(o: &indexmap::IndexMap<JsonString, Revision>) -> Vec<&JsonString> {
    let mut indices = Vec::new();
    let mut others = Vec::new();
    for k in o.keys() {
        let index = k.to_utf8().ok().and_then(|s| {
            s.parse::<u32>()
                .ok()
                .filter(|n| *n < u32::MAX && n.to_string() == s)
        });
        if let Some(n) = index {
            indices.push((n, k))
        } else {
            others.push(k)
        }
    }
    indices.sort_unstable_by_key(|(n, _)| *n);
    indices.into_iter().map(|(_, k)| k).chain(others).collect()
}
fn subsequence(a: &[Revision], b: &[Revision]) -> Option<Vec<(usize, usize)>> {
    if a.len() == b.len() {
        return None;
    }
    let mut matches = Vec::new();
    let mut at = 0;
    if b.len() < a.len() {
        for (j, v) in b.iter().enumerate() {
            while at < a.len() && !a[at].same(v) {
                at += 1
            }
            if at == a.len() {
                return None;
            }
            matches.push((at, j));
            at += 1
        }
    } else {
        for (i, v) in a.iter().enumerate() {
            while at < b.len() && !b[at].same(v) {
                at += 1
            }
            if at == b.len() {
                return None;
            }
            matches.push((i, at));
            at += 1
        }
    }
    Some(matches)
}
fn anchors(a: &[Revision], b: &[Revision]) -> Vec<(usize, usize)> {
    let mut positions: HashMap<Identity, Vec<usize>> = HashMap::new();
    for (i, v) in a.iter().enumerate() {
        positions.entry(identity(v)).or_default().push(i)
    }
    let count: usize = b
        .iter()
        .map(|v| positions.get(&identity(v)).map_or(0, Vec::len))
        .sum();
    if count > 200_000 {
        let mut next = 0;
        let mut matches = Vec::new();
        for (j, v) in b.iter().enumerate() {
            if let Some(p) = positions.get(&identity(v)) {
                let at = p.partition_point(|i| *i < next);
                if let Some(i) = p.get(at) {
                    matches.push((*i, j));
                    next = i + 1
                }
            }
        }
        return matches;
    }
    let mut candidates: Vec<(usize, usize, Option<usize>)> = Vec::with_capacity(count);
    let mut tails: Vec<usize> = Vec::new();
    let mut values = Vec::new();
    for (j, v) in b.iter().enumerate() {
        if let Some(p) = positions.get(&identity(v)) {
            for &i in p.iter().rev() {
                let at = values.partition_point(|n| *n < i);
                let previous = at.checked_sub(1).map(|n| tails[n]);
                let candidate = candidates.len();
                candidates.push((i, j, previous));
                if at == tails.len() {
                    tails.push(candidate);
                    values.push(i)
                } else {
                    tails[at] = candidate;
                    values[at] = i
                }
            }
        }
    }
    let mut matches = Vec::new();
    let mut at = tails.last().copied();
    while let Some(i) = at {
        let (i, j, prev) = candidates[i];
        matches.push((i, j));
        at = prev
    }
    matches.reverse();
    matches
}
fn aligned(a: &Revision, b: &Revision) -> bool {
    if a.equal(b) {
        return true;
    }
    match (a.0.as_ref(), b.0.as_ref()) {
        (Node::Array(a), Node::Array(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b)
                    .any(|(a, b)| !matches!(a.0.as_ref(), Node::Scalar(_)) && a.ptr_eq(b))
        }
        (Node::Object(a), Node::Object(b)) => a.iter().any(|(k, a)| {
            !matches!(a.0.as_ref(), Node::Scalar(_)) && b.get(k).is_some_and(|b| a.ptr_eq(b))
        }),
        _ => false,
    }
}
fn semantic(a: &[Revision], b: &[Revision]) -> Vec<(usize, usize)> {
    if a.len().saturating_mul(b.len()) > 65_536 {
        return Vec::new();
    }
    let width = b.len() + 1;
    let mut lengths = vec![0u32; (a.len() + 1) * width];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lengths[i * width + j] = if aligned(&a[i], &b[j]) {
                lengths[(i + 1) * width + j + 1] + 1
            } else {
                lengths[(i + 1) * width + j].max(lengths[i * width + j + 1])
            }
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut matches = Vec::new();
    while i < a.len() && j < b.len() {
        if aligned(&a[i], &b[j]) && lengths[i * width + j] == lengths[(i + 1) * width + j + 1] + 1 {
            matches.push((i, j));
            i += 1;
            j += 1
        } else if lengths[(i + 1) * width + j] >= lengths[i * width + j + 1] {
            i += 1
        } else {
            j += 1
        }
    }
    matches
}
fn json_cost(v: &JsonValue) -> usize {
    match v {
        JsonValue::Null => 4,
        JsonValue::Bool(b) => {
            if *b {
                4
            } else {
                5
            }
        }
        JsonValue::Number(n) => n.to_string().len(),
        JsonValue::String(s) => s.0.len() + 2,
        JsonValue::Array(a) => {
            2 + a.iter().map(json_cost).sum::<usize>() + a.len().saturating_sub(1)
        }
        JsonValue::Object(o) => {
            2 + o
                .iter()
                .map(|(k, v)| k.0.len() + 3 + json_cost(v))
                .sum::<usize>()
                + o.len().saturating_sub(1)
        }
    }
}
fn path_cost(p: &Path) -> usize {
    2 + p
        .iter()
        .map(|s| match s {
            Segment::Key(s) => s.0.len() + 2,
            Segment::Index(n) => n.to_string().len(),
        })
        .sum::<usize>()
        + p.len().saturating_sub(1)
}
fn operation_cost(op: &Op) -> usize {
    match op {
        Op::Replace(v) => 6 + json_cost(v),
        Op::Set(p, v) => 7 + path_cost(p) + json_cost(v),
        Op::Delete(p) => 6 + path_cost(p),
        Op::Append(p, s) => 9 + path_cost(p) + s.0.len(),
        Op::Truncate(p, n) => 7 + path_cost(p) + n.to_string().len(),
        Op::Splice(p, i, r, a) => {
            10 + path_cost(p)
                + i.to_string().len()
                + r.to_string().len()
                + json_cost(&JsonValue::Array(a.clone()))
        }
        Op::Permute(p, a) => {
            7 + path_cost(p)
                + 2
                + a.iter().map(|i| i.to_string().len()).sum::<usize>()
                + a.len().saturating_sub(1)
        }
    }
}
pub fn diff_shared(before: &Revision, after: &Revision) -> Vec<Op> {
    let mut differ = Differ {
        ops: Vec::new(),
        overflow: false,
    };
    differ.value(before, after, Vec::new());
    if differ.overflow {
        return vec![Op::Replace(after.to_json())];
    }
    if differ.ops.is_empty() || matches!(differ.ops.first(), Some(Op::Replace(_))) {
        return differ.ops;
    }
    let cost = 2 + differ
        .ops
        .iter()
        .map(|op| operation_cost(op) + 1)
        .sum::<usize>();
    if cost >= 65_536 && cost >= json_cost(&after.to_json()) + 6 {
        vec![Op::Replace(after.to_json())]
    } else {
        differ.ops
    }
}
pub fn diff_revisions(before: &JsonValue, after: &JsonValue) -> DeltaResult<Vec<Op>> {
    Ok(diff_shared(
        &Revision::from_json(before.clone())?,
        &Revision::from_json(after.clone())?,
    ))
}
