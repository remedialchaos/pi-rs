//! The command menu's route layers (upstream `command-menu/layers.ts`).

/// A command target: the row/main area the menu acts on. All four fields are strings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandTarget {
    pub target_type: String,
    pub id: String,
    pub kind: String,
    pub label: String,
}

impl CommandTarget {
    fn is_valid(value: &serde_json::Value) -> bool {
        let Some(object) = value.as_object() else {
            return false;
        };
        ["type", "id", "kind", "label"]
            .iter()
            .all(|key| object.get(*key).and_then(|v| v.as_str()).is_some())
    }

    fn from_json(value: &serde_json::Value) -> Option<Self> {
        let object = value.as_object()?;
        Some(Self {
            target_type: object.get("type")?.as_str()?.to_owned(),
            id: object.get("id")?.as_str()?.to_owned(),
            kind: object.get("kind")?.as_str()?.to_owned(),
            label: object.get("label")?.as_str()?.to_owned(),
        })
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "type": self.target_type,
            "id": self.id,
            "kind": self.kind,
            "label": self.label,
        })
    }
}

/// One route layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layer {
    pub kind: String,
    pub name: String,
    pub locator: Option<String>,
}

/// The portion of a route needed by the menu.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Route {
    pub layers: Vec<Layer>,
}

/// The open menu as its layers say: the captured target and the pages opened on top.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenMenu {
    pub target: Option<CommandTarget>,
    pub pages: Vec<Option<String>>,
}

const NAME: &str = "commands";

/// Where the trailing run of menu layers starts (`layers.len()` when the menu is closed).
pub fn menu_start(layers: &[Layer]) -> usize {
    let mut start = layers.len();
    while start > 0 && is_menu_layer(&layers[start - 1]) {
        start -= 1;
    }
    start
}

fn is_menu_layer(layer: &Layer) -> bool {
    layer.kind == "sheet" && layer.name == NAME
}

/// The root layer of a menu (optionally opened straight onto `page`).
pub fn root_layer(target: Option<&CommandTarget>, page: Option<&str>) -> Layer {
    let mut object = serde_json::Map::new();
    if let Some(target) = target {
        object.insert("target".into(), target.to_json());
    }
    if let Some(page) = page {
        object.insert("page".into(), serde_json::Value::String(page.to_owned()));
    }
    Layer {
        kind: "sheet".into(),
        name: NAME.into(),
        locator: Some(serde_json::Value::Object(object).to_string()),
    }
}

/// A page layer opened on top of the menu's layers.
pub fn menu_page_layer(name: &str) -> Layer {
    Layer {
        kind: "sheet".into(),
        name: NAME.into(),
        locator: Some(serde_json::json!({ "page": name }).to_string()),
    }
}

/// Reads a locator from untrusted history/session storage: anything not written by this
/// module reads as empty.
fn parse(layer: &Layer) -> (Option<CommandTarget>, Option<String>) {
    if layer.kind != "sheet" {
        return (None, None);
    }
    let Some(locator) = layer.locator.as_deref() else {
        return (None, None);
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(locator) else {
        return (None, None);
    };
    let Some(object) = value.as_object() else {
        return (None, None);
    };
    let target = object
        .get("target")
        .filter(|v| CommandTarget::is_valid(v))
        .and_then(CommandTarget::from_json);
    let page = object
        .get("page")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    (target, page)
}

/// The open menu, when the route's top layer is one of its layers.
pub fn open_menu(route: &Route) -> Option<OpenMenu> {
    let start = menu_start(&route.layers);
    if start == route.layers.len() {
        return None;
    }
    let parsed: Vec<(Option<CommandTarget>, Option<String>)> =
        route.layers[start..].iter().map(parse).collect();
    let (root_target, root_page) = parsed.first()?.clone();
    let mut pages = Vec::new();
    if let Some(page) = root_page {
        pages.push(Some(page));
    }
    pages.extend(parsed.iter().skip(1).map(|(_, page)| page.clone()));
    Some(OpenMenu {
        target: root_target,
        pages,
    })
}

/// Pushes a menu root layer onto a route (upstream `pushMenu`).
pub fn push_menu(route: &Route, target: Option<&CommandTarget>) -> Route {
    let mut layers = route.layers.clone();
    layers.push(root_layer(target, None));
    Route { layers }
}

/// Opens the menu right on `page` for `target`: one layer, so Back closes it.
pub fn open_command_page(route: &Route, page: &str, target: Option<&CommandTarget>) -> Route {
    let mut layers = route.layers.clone();
    layers.push(root_layer(target, Some(page)));
    Route { layers }
}

/// Closes the menu: removes its layers and nothing under them.
pub fn close_menu(route: &Route) -> Route {
    let start = menu_start(&route.layers);
    Route {
        layers: route.layers[..start].to_vec(),
    }
}

/// Opens `name` on top of the menu's layers.
pub fn push_menu_page(route: &Route, name: &str) -> Route {
    let mut layers = route.layers.clone();
    layers.push(menu_page_layer(name));
    Route { layers }
}

/// Goes up one page; from the root it closes the menu.
pub fn pop_menu(route: &Route) -> Route {
    let mut layers = route.layers.clone();
    layers.pop();
    Route { layers }
}

/// Drops the captured target from the menu's root layer, in place.
pub fn drop_menu_target(route: &Route) -> Route {
    let start = menu_start(&route.layers);
    let layers = route
        .layers
        .iter()
        .enumerate()
        .map(|(index, layer)| {
            if index == start {
                let (_, page) = parse(layer);
                root_layer(None, page.as_deref())
            } else {
                layer.clone()
            }
        })
        .collect();
    Route { layers }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(id: &str) -> CommandTarget {
        CommandTarget {
            target_type: "session".into(),
            id: id.into(),
            kind: "conversation".into(),
            label: "Demo".into(),
        }
    }

    #[test]
    fn menu_start_finds_trailing_run() {
        let route = push_menu(&Route::default(), Some(&target("s1")));
        assert_eq!(menu_start(&route.layers), 0);
        assert_eq!(close_menu(&route), Route::default());
    }

    #[test]
    fn open_menu_reads_target_and_pages() {
        let mut route = push_menu(&Route::default(), Some(&target("s1")));
        route = push_menu_page(&route, "models");
        let menu = open_menu(&route).unwrap();
        assert_eq!(menu.target.as_ref().map(|t| t.id.as_str()), Some("s1"));
        assert_eq!(menu.pages, vec![Some("models".to_owned())]);
    }

    #[test]
    fn command_page_is_one_layer_and_back_closes() {
        let route = open_command_page(&Route::default(), "themes", Some(&target("s1")));
        assert_eq!(route.layers.len(), 1);
        assert_eq!(
            open_menu(&route).unwrap().pages,
            vec![Some("themes".to_owned())]
        );
        assert_eq!(pop_menu(&route), Route::default());
    }

    #[test]
    fn untrusted_locators_read_as_empty() {
        let bad = Layer {
            kind: "sheet".into(),
            name: NAME.into(),
            locator: Some("not json".into()),
        };
        let route = Route { layers: vec![bad] };
        let menu = open_menu(&route).unwrap();
        assert!(menu.target.is_none());
        assert!(menu.pages.is_empty());
    }

    #[test]
    fn drop_target_keeps_page() {
        let route = open_command_page(&Route::default(), "themes", Some(&target("s1")));
        let dropped = drop_menu_target(&route);
        let menu = open_menu(&dropped).unwrap();
        assert!(menu.target.is_none());
        assert_eq!(menu.pages, vec![Some("themes".to_owned())]);
    }
}
