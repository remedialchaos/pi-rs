//! Artifacts viewer layer (`apps/p2/plugins/artifacts/viewer.ts`): parses and constructs
//! route layers showing a sent file (`send_file`) over the chat transcript.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::runtime::files::DownloadInfo;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteLayer {
    pub kind: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locator: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewerFileInfo {
    pub id: String,
    pub name: String,
    pub size: u64,
}

impl From<&DownloadInfo> for ViewerFileInfo {
    fn from(info: &DownloadInfo) -> Self {
        Self {
            id: info.id.clone(),
            name: info.name.clone(),
            size: info.size,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewerItem {
    pub kind: String,
    pub file: ViewerFileInfo,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

impl ViewerItem {
    pub fn file(file: ViewerFileInfo) -> Self {
        Self {
            kind: "file".into(),
            file,
            extra: serde_json::Map::new(),
        }
    }
}

/// Builds the route layer displaying `item`.
pub fn viewer_layer(item: &ViewerItem) -> serde_json::Result<RouteLayer> {
    let encoded = serde_json::to_string(item)?;
    Ok(RouteLayer {
        kind: "viewer".into(),
        name: "artifacts".into(),
        locator: Some(format!("viewer:{encoded}")),
    })
}

/// Extracts and validates the artifacts viewer item from the route's top layer, if present.
pub fn viewer_item(top: Option<&RouteLayer>) -> Option<ViewerItem> {
    let layer = top?;
    if layer.kind != "viewer" || layer.name != "artifacts" {
        return None;
    }
    parse(layer.locator.as_deref())
}

fn parse(locator: Option<&str>) -> Option<ViewerItem> {
    let raw = locator?.strip_prefix("viewer:")?;
    let parsed: Value = serde_json::from_str(raw).ok()?;
    let obj = parsed.as_object()?;
    if obj.get("kind").and_then(Value::as_str) != Some("file") {
        return None;
    }
    let file = obj.get("file")?.as_object()?;
    let id = file.get("id")?.as_str()?;
    let name = file.get("name")?.as_str()?;
    let size = file.get("size")?.as_u64()?;
    if id.is_empty() || name.is_empty() {
        return None;
    }
    let mut extra = obj.clone();
    extra.remove("kind");
    extra.remove("file");
    Some(ViewerItem {
        kind: "file".into(),
        file: ViewerFileInfo {
            id: id.to_owned(),
            name: name.to_owned(),
            size,
        },
        extra,
    })
}
