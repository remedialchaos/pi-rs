use super::*;
use serde_json::Value;

pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Copy, Debug)]
pub struct ScanStart {
    pub order: ScanOrder,
    pub after: Option<i64>,
}
impl ScanStart {
    pub fn contains(self, id: u64) -> bool {
        self.after.is_none_or(|after| match self.order {
            ScanOrder::Ascending => i128::from(id) > i128::from(after),
            ScanOrder::Descending => i128::from(id) < i128::from(after),
        })
    }
}

pub fn scan_start(
    requested: Option<ScanOrder>,
    cursor: Option<&Cursor>,
    fallback: ScanOrder,
) -> StorageResult<ScanStart> {
    let Some(cursor) = cursor else {
        return Ok(ScanStart {
            order: requested.unwrap_or(fallback),
            after: None,
        });
    };
    let bad = || StorageError::Request("Invalid storage cursor".into());
    let after = cursor
        .get("after")
        .and_then(cursor_number)
        .ok_or_else(bad)?;
    let order = match cursor.get("order") {
        None => fallback,
        Some(value) => match value.as_str() {
            Some("ascending") => ScanOrder::Ascending,
            Some("descending") => ScanOrder::Descending,
            _ => return Err(bad()),
        },
    };
    if let Some(requested) = requested
        && requested != order
    {
        return Err(StorageError::Request(format!(
            "The cursor continues a {} scan; the query asks for {}",
            order.as_str(),
            requested.as_str()
        )));
    }
    Ok(ScanStart {
        order,
        after: Some(after),
    })
}

pub(crate) fn document_scan_start(cursor: Option<&Cursor>) -> StorageResult<ScanStart> {
    let after = cursor
        .and_then(|cursor| cursor.get("after"))
        .map(|value| {
            cursor_number(value)
                .ok_or_else(|| StorageError::Request("Invalid storage cursor".into()))
        })
        .transpose()?;
    Ok(ScanStart {
        order: ScanOrder::Ascending,
        after,
    })
}

fn cursor_number(value: &Value) -> Option<i64> {
    if let Some(n) = value.as_i64() {
        return (n.unsigned_abs() <= MAX_SAFE_INTEGER).then_some(n);
    }
    let n = value.as_f64()?;
    (n.fract() == 0.0 && n.abs() <= MAX_SAFE_INTEGER as f64).then_some(n as i64)
}

pub fn next_cursor(id: u64, order: ScanOrder) -> Cursor {
    Cursor::from_iter([
        ("after".into(), Value::from(id)),
        ("order".into(), Value::from(order.as_str())),
    ])
}

pub(crate) fn paginate<T>(
    mut values: impl Iterator<Item = T>,
    id: impl Fn(&T) -> u64,
    limit: usize,
    order: ScanOrder,
) -> Page<T> {
    let items: Vec<_> = values.by_ref().take(limit).collect();
    let next = if values.next().is_some() {
        items.last().map(|item| next_cursor(id(item), order))
    } else {
        None
    };
    Page { items, next }
}
