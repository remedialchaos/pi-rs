//! Draft edit provenance: placement boundaries, object insertion order and stable array origins.
use super::{revision::Node, *};
use indexmap::{IndexMap, IndexSet};
#[derive(Default)]
struct Meta {
    revision: Option<WeakRevision>,
    order: usize,
    writes: IndexMap<JsonString, Option<JsonString>>,
    deletes: IndexSet<JsonString>,
    readded: IndexSet<JsonString>,
    origins: Vec<Option<usize>>,
    structural: bool,
    overrides: IndexSet<usize>,
}
#[derive(Default)]
pub(super) struct Journal {
    nodes: HashMap<usize, Meta>,
    clock: usize,
}
struct Emission {
    depth: usize,
    order: usize,
    ops: Vec<Op>,
}
impl Journal {
    pub fn record(&mut self, before: &Revision, after: &Revision, op: &Op) -> DeltaResult<()> {
        self.nodes.retain(|_, meta| {
            meta.revision
                .as_ref()
                .and_then(WeakRevision::upgrade)
                .is_some()
        });
        self.clock += 1;
        let Some(path) = op.path() else {
            self.nodes.clear();
            return Ok(());
        };
        let target_len = if matches!(op, Op::Set(..) | Op::Delete(_)) {
            path.len() - 1
        } else {
            path.len()
        };
        let mut transferred = Vec::new();
        for depth in 0..=target_len {
            let old = before.at(&path[..depth])?;
            let new = after.at(&path[..depth])?;
            if !old.is_container() {
                continue;
            }
            let mut meta = self.nodes.remove(&old.identity()).unwrap_or_else(|| Meta {
                order: self.clock,
                origins: old
                    .elements()
                    .map_or_else(Vec::new, |a| (0..a.len()).map(Some).collect()),
                ..Meta::default()
            });
            if depth == target_len {
                match (old.0.as_ref(), op) {
                    (Node::Object(_), Op::Set(path, _)) => {
                        let key = object_key(&path[path.len() - 1]);
                        if meta.deletes.shift_remove(&key) {
                            meta.readded.insert(key.clone());
                        }
                        meta.writes.insert(key, None);
                    }
                    (Node::Object(_), Op::Delete(path)) => {
                        let key = object_key(&path[path.len() - 1]);
                        meta.writes.shift_remove(&key);
                        meta.readded.shift_remove(&key);
                        meta.deletes.insert(key);
                    }
                    (Node::Array(a), Op::Set(path, _)) => {
                        if let Some(Segment::Index(index)) = path.last() {
                            if *index == a.len() {
                                meta.origins.push(None);
                                meta.structural = true;
                            } else if let Some(Some(origin)) = meta.origins.get(*index) {
                                meta.overrides.insert(*origin);
                            }
                        }
                    }
                    (Node::Array(a), Op::Splice(_, index, remove, items)) => {
                        let start = (*index).min(a.len());
                        let end = start + (*remove).min(a.len() - start);
                        meta.origins.splice(start..end, vec![None; items.len()]);
                        meta.structural = true;
                    }
                    (Node::Array(_), Op::Permute(_, order)) => {
                        let previous = meta.origins.clone();
                        meta.origins = order.iter().map(|i| previous[*i]).collect();
                        meta.structural = true;
                    }
                    _ => {}
                }
            }
            meta.revision = Some(new.downgrade());
            transferred.push((new.identity(), meta));
        }
        // appendText is distinguished from an assignment: its payload is accumulated without rescanning the base.
        if let Op::Append(path, text) = op
            && let Some((key, parents)) = path.split_last()
        {
            let parent = after.at(parents)?;
            if let Some((_, meta)) = transferred
                .iter_mut()
                .find(|(id, _)| *id == parent.identity())
                && matches!(parent.0.as_ref(), Node::Object(_))
            {
                let entry = meta
                    .writes
                    .entry(object_key(key))
                    .or_insert_with(|| Some(JsonString(Vec::new())));
                if let Some(appended) = entry {
                    appended.0.extend(&text.0);
                }
            }
        }
        self.nodes.extend(transferred);
        Ok(())
    }
    pub fn emit(&self, before: &Revision, after: &Revision) -> Vec<Op> {
        let mut emitted = Vec::new();
        self.visit(before, after, Vec::new(), &mut emitted);
        emitted.sort_by_key(|e| (e.depth, e.order));
        let ops = emitted.into_iter().flat_map(|e| e.ops).collect::<Vec<_>>();
        if ops.len() > MAX_DELTA_OPERATIONS {
            vec![Op::Replace(after.to_json())]
        } else {
            ops
        }
    }
    fn visit(&self, before: &Revision, after: &Revision, path: Path, out: &mut Vec<Emission>) {
        if before.ptr_eq(after) {
            return;
        }
        let meta = self.nodes.get(&after.identity());
        let order = meta.map_or(usize::MAX, |m| m.order);
        match (before.0.as_ref(), after.0.as_ref()) {
            (Node::Object(a), Node::Object(b)) => {
                // Unsafe own-key edits fold the containing object rather than ever emitting an unsafe path.
                let unsafe_change = meta.is_some_and(|m| {
                    m.writes
                        .keys()
                        .chain(m.deletes.iter())
                        .chain(m.readded.iter())
                        .any(reserved)
                }) || a.keys().chain(b.keys()).any(|key| {
                    reserved(key)
                        && !match (a.get(key), b.get(key)) {
                            (Some(a), Some(b)) => a.ptr_eq(b),
                            (None, None) => true,
                            _ => false,
                        }
                });
                if unsafe_change {
                    out.push(Emission {
                        depth: path.len(),
                        order,
                        ops: vec![placement(path, after)],
                    });
                    return;
                }
                let Some(meta) = meta else {
                    if !before.equal(after) {
                        out.push(Emission {
                            depth: path.len(),
                            order,
                            ops: vec![placement(path, after)],
                        })
                    }
                    return;
                };
                let mut ops = Vec::new();
                for key in &meta.readded {
                    if a.contains_key(key) {
                        ops.push(Op::Delete(child(&path, Segment::Key(key.clone()))));
                    }
                }
                for (key, append) in &meta.writes {
                    let Some(value) = b.get(key) else { continue };
                    let p = child(&path, Segment::Key(key.clone()));
                    if let Some(append) = append {
                        if !append.0.is_empty() {
                            ops.push(Op::Append(p, append.clone()));
                        }
                    } else if meta.readded.contains(key) {
                        ops.push(Op::Set(p, value.to_json()));
                    } else {
                        changed(a.get(key), value, p, &mut ops);
                    }
                }
                for key in &meta.deletes {
                    if a.contains_key(key) {
                        ops.push(Op::Delete(child(&path, Segment::Key(key.clone()))));
                    }
                }
                if !ops.is_empty() {
                    out.push(Emission {
                        depth: path.len(),
                        order,
                        ops,
                    });
                }
                for (key, value) in b {
                    if !meta.writes.contains_key(key)
                        && let Some(old) = a.get(key)
                    {
                        self.visit(old, value, child(&path, Segment::Key(key.clone())), out)
                    }
                }
            }
            (Node::Array(a), Node::Array(b)) => {
                let Some(meta) = meta else {
                    if !before.equal(after) {
                        out.push(Emission {
                            depth: path.len(),
                            order,
                            ops: vec![placement(path, after)],
                        })
                    }
                    return;
                };
                if !meta.structural
                    && a.len() == b.len()
                    && a.iter().zip(b).all(|(a, b)| a.equal(b))
                {
                    return;
                }
                let mut ops = Vec::new();
                let mut covered = HashSet::new();
                if !meta.structural {
                    let changed = (0..b.len())
                        .filter(|i| a.get(*i).is_none_or(|old| !old.equal(&b[*i])))
                        .collect::<HashSet<_>>();
                    if changed.len() >= 256 {
                        let mut i = 0;
                        while i < b.len() {
                            while i < b.len() && !changed.contains(&i) {
                                i += 1
                            }
                            let start = i;
                            let mut end = i;
                            let mut count = 0;
                            let mut gap = 0;
                            while i < b.len() {
                                if changed.contains(&i) {
                                    count += 1;
                                    end = i;
                                    gap = 0;
                                } else {
                                    gap += 1;
                                    if gap > 1 {
                                        break;
                                    }
                                }
                                i += 1;
                            }
                            if count >= 256 && count * 2 > end - start {
                                ops.push(Op::Splice(
                                    path.clone(),
                                    start,
                                    end - start + 1,
                                    b[start..=end].iter().map(Revision::to_json).collect(),
                                ));
                                covered.extend(start..=end);
                            }
                            if i == start {
                                i += 1;
                            }
                        }
                    }
                } else {
                    let retained = meta
                        .origins
                        .iter()
                        .flatten()
                        .copied()
                        .collect::<HashSet<_>>();
                    let mut end = a.len();
                    while end > 0 {
                        if retained.contains(&(end - 1)) {
                            end -= 1;
                            continue;
                        }
                        let mut start = end - 1;
                        while start > 0 && !retained.contains(&(start - 1)) {
                            start -= 1
                        }
                        ops.push(Op::Splice(path.clone(), start, end - start, Vec::new()));
                        end = start;
                    }
                    let mut kept = retained.iter().copied().collect::<Vec<_>>();
                    kept.sort_unstable();
                    let positions = kept
                        .iter()
                        .enumerate()
                        .map(|(i, base)| (*base, i))
                        .collect::<HashMap<_, _>>();
                    let target = meta.origins.iter().flatten().copied().collect::<Vec<_>>();
                    if target != kept {
                        ops.push(Op::Permute(
                            path.clone(),
                            target.iter().map(|base| positions[base]).collect(),
                        ));
                    }
                    let mut at = 0;
                    while at < meta.origins.len() {
                        if meta.origins[at].is_some() {
                            at += 1;
                            continue;
                        }
                        let start = at;
                        while at < meta.origins.len() && meta.origins[at].is_none() {
                            at += 1
                        }
                        ops.push(Op::Splice(
                            path.clone(),
                            start,
                            0,
                            b[start..at].iter().map(Revision::to_json).collect(),
                        ));
                    }
                }
                for origin in &meta.overrides {
                    if let Some(index) = meta.origins.iter().position(|v| *v == Some(*origin))
                        && !covered.contains(&index)
                    {
                        changed(
                            a.get(*origin),
                            &b[index],
                            child(&path, Segment::Index(index)),
                            &mut ops,
                        );
                    }
                }
                if !ops.is_empty() {
                    out.push(Emission {
                        depth: path.len(),
                        order,
                        ops,
                    });
                }
                for (index, value) in b.iter().enumerate() {
                    let Some(origin) = meta.origins.get(index).copied().flatten() else {
                        continue;
                    };
                    if covered.contains(&index) || meta.overrides.contains(&origin) {
                        continue;
                    }
                    if let Some(old) = a.get(origin) {
                        self.visit(old, value, child(&path, Segment::Index(index)), out)
                    }
                }
            }
            _ => {
                let mut ops = Vec::new();
                changed(Some(before), after, path.clone(), &mut ops);
                if !ops.is_empty() {
                    out.push(Emission {
                        depth: path.len().saturating_sub(1),
                        order,
                        ops,
                    })
                }
            }
        }
    }
}
fn child(path: &[Segment], segment: Segment) -> Path {
    let mut p = path.to_vec();
    p.push(segment);
    p
}
fn placement(path: Path, value: &Revision) -> Op {
    if path.is_empty() {
        Op::Replace(value.to_json())
    } else {
        Op::Set(path, value.to_json())
    }
}
fn changed(before: Option<&Revision>, after: &Revision, path: Path, ops: &mut Vec<Op>) {
    if before.is_some_and(|b| b.equal(after)) {
        return;
    }
    if let (Some(before), Node::Scalar(JsonValue::String(b))) = (before, after.0.as_ref())
        && let Node::Scalar(JsonValue::String(a)) = before.0.as_ref()
    {
        if b.0.len() > a.0.len() && b.0.starts_with(&a.0) {
            ops.push(Op::Append(path, JsonString(b.0[a.0.len()..].to_vec())));
            return;
        }
        let shared = overlap(a, b, 65_536);
        if shared > 0 {
            ops.push(Op::Truncate(path.clone(), a.0.len() - shared));
            if b.0.len() > shared {
                ops.push(Op::Append(path, JsonString(b.0[shared..].to_vec())))
            }
            return;
        }
    }
    ops.push(placement(path, after));
}
