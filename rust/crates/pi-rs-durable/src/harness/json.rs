use super::super::session::Result;
use pi_rs_chord::delta::{Change, Segment};
use serde_json::Value;
use std::collections::HashMap;

pub enum AssignPlan {
    Append(String),
    Keep,
    Fields(HashMap<Segment, AssignPlan>),
}

pub fn assign_json(
    draft: &mut Change,
    path: &mut Vec<Segment>,
    current: Option<&Value>,
    value: &Value,
    plan: Option<&AssignPlan>,
) -> Result<()> {
    if matches!(plan, Some(AssignPlan::Keep)) {
        return Ok(());
    }
    let child_plan = |key: &Segment| match plan {
        Some(AssignPlan::Fields(fields)) => fields.get(key),
        _ => None,
    };
    match (current, value) {
        (Some(Value::Object(old)), Value::Object(new)) => {
            for name in old.keys().filter(|name| !new.contains_key(*name)) {
                path.push(Segment::Key(name.as_str().into()));
                draft.delete(path)?;
                path.pop();
            }
            for (name, child) in new {
                let key = Segment::Key(name.as_str().into());
                let nested = child_plan(&key);
                path.push(key);
                assign_json(draft, path, old.get(name), child, nested)?;
                path.pop();
            }
        }
        (Some(Value::Array(old)), Value::Array(new)) if old.len() <= new.len() => {
            for (index, (was, child)) in old.iter().zip(new).enumerate() {
                let key = Segment::Index(index);
                let nested = child_plan(&key);
                path.push(key);
                assign_json(draft, path, Some(was), child, nested)?;
                path.pop();
            }
            if new.len() > old.len() {
                draft.splice(
                    path,
                    old.len(),
                    0,
                    new[old.len()..].iter().cloned().map(Into::into).collect(),
                )?;
            }
        }
        (Some(Value::String(old)), Value::String(new))
            if matches!(path.last(), Some(Segment::Key(_))) =>
        {
            if let Some(AssignPlan::Append(text)) = plan
                && old.encode_utf16().count() + text.encode_utf16().count()
                    == new.encode_utf16().count()
            {
                draft.append_text(path, text.as_str().into())?;
            } else if old != new {
                if let Some(text) = new.strip_prefix(old) {
                    draft.append_text(path, text.into())?;
                } else {
                    draft.set(path, value.clone().into())?;
                }
            }
        }
        _ if current != Some(value) => draft.set(path, value.clone().into())?,
        _ => {}
    }
    Ok(())
}
