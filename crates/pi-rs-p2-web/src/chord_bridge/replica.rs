type Result<T> = std::result::Result<T, String>;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Default)]
pub struct Replica {
    pub value: Value,
    paths: HashMap<u64, Vec<Value>>,
}
fn index(v: &Value) -> Result<usize> {
    v.as_u64()
        .and_then(|v| usize::try_from(v).ok())
        .ok_or_else(|| "invalid delta index".into())
}
fn target<'a>(mut value: &'a mut Value, path: &[Value]) -> Result<&'a mut Value> {
    for seg in path {
        value = if let Some(key) = seg.as_str() {
            value.get_mut(key)
        } else {
            value.get_mut(index(seg)?)
        }
        .ok_or("missing delta target")?;
    }
    Ok(value)
}
impl Replica {
    pub fn snapshot(value: Value) -> Self {
        Self {
            value,
            ..Self::default()
        }
    }
    pub fn apply(&mut self, changes: Value) -> Result<()> {
        let mut previous = None;
        for op in changes.as_array().ok_or("expected list of deltas")? {
            let fields = op.as_array().ok_or("expected delta tuple")?;
            let kind = op[0].as_str().ok_or("missing delta kind")?;
            let arity = match kind {
                "r" => {
                    self.value = fields.get(1).ok_or("missing replacement")?.clone();
                    self.paths.clear();
                    previous = None;
                    continue;
                }
                "#" => {
                    self.paths.insert(
                        op[1].as_u64().ok_or("invalid path id")?,
                        op[2].as_array().ok_or("invalid path")?.clone(),
                    );
                    continue;
                }
                "p" => 5,
                "d" => 2,
                "s" | "m" | "t" | "a" => 3,
                _ => return Err("unknown delta".into()),
            };
            let short = fields.len() + 1 == arity;
            if !short {
                if fields.len() != arity {
                    return Err("invalid delta arity".into());
                }
                previous = Some(
                    match op[1].as_u64() {
                        Some(id) => self.paths.get(&id).ok_or("unknown path id")?,
                        None => op[1].as_array().ok_or("invalid delta path")?,
                    }
                    .clone(),
                );
            }
            let path = previous.as_deref().ok_or("missing previous path")?;
            let args = &fields[if short { 1 } else { 2 }..];
            if kind == "s" || kind == "d" {
                let (key, parent) = path.split_last().ok_or("missing key")?;
                let parent = target(&mut self.value, parent)?;
                if let Some(map) = parent.as_object_mut() {
                    let key = key.as_str().ok_or("invalid object key")?;
                    if kind == "s" {
                        map.insert(key.into(), args[0].clone());
                    } else {
                        map.remove(key);
                    }
                } else {
                    let array = parent.as_array_mut().ok_or("invalid delta parent")?;
                    let i = index(key)?;
                    if kind == "s" && i == array.len() {
                        array.push(args[0].clone());
                        continue;
                    }
                    if i >= array.len() {
                        return Err("index outside array".into());
                    }
                    if kind == "s" {
                        array[i] = args[0].clone()
                    } else {
                        array.remove(i);
                    }
                }
                continue;
            }
            let value = target(&mut self.value, path)?;
            match kind {
                "a" => {
                    let mut text = value.as_str().ok_or("expected text")?.to_owned();
                    text.push_str(args[0].as_str().ok_or("invalid append")?);
                    *value = Value::String(text);
                }
                "t" => {
                    let n = index(&args[0])?;
                    let units = value
                        .as_str()
                        .ok_or("expected text")?
                        .encode_utf16()
                        .skip(n)
                        .collect::<Vec<_>>();
                    *value = Value::String(String::from_utf16_lossy(&units));
                }
                "p" => {
                    let items = value.as_array_mut().ok_or("expected array")?;
                    let start = index(&args[0])?;
                    let end = start
                        .checked_add(index(&args[1])?)
                        .filter(|end| *end <= items.len())
                        .ok_or("invalid splice")?;
                    items.splice(
                        start..end,
                        args[2]
                            .as_array()
                            .ok_or("invalid splice items")?
                            .iter()
                            .cloned(),
                    );
                }
                "m" => {
                    let items = value.as_array().ok_or("expected array")?;
                    let reordered = args[0]
                        .as_array()
                        .ok_or("invalid permutation")?
                        .iter()
                        .map(|v| {
                            items
                                .get(index(v)?)
                                .cloned()
                                .ok_or_else(|| "invalid permutation index".into())
                        })
                        .collect::<Result<_>>()?;
                    *value = Value::Array(reordered);
                }
                _ => return Err("unknown delta".into()),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn streaming_updates_reuse_the_member_path_across_frames() -> Result<()> {
        let mut replica = Replica::snapshot(json!({"text":"draft "}));
        replica.apply(json!([["#", 1, ["text"]], ["a", 1, "😀"], ["a", " live"]]))?;
        replica.apply(json!([["t", 1, 6]]))?;
        assert_eq!(replica.value, json!({"text":"😀 live"}));
        Ok(())
    }
}
