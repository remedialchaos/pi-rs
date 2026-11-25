use crate::{Error, Result, appearance::Appearance};
use serde::Serialize;
use serde_json::Value;
use url::Url;

pub const HANDLER: &str = "piHost";
pub const THEME_EVENT: &str = "pi-host-theme";
pub const MENU_EVENT: &str = "pi-host-menu";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Message {
    Theme,
    SetTheme(Appearance),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Theme {
    pub preference: Appearance,
}

pub fn owns(expected: &Url, actual: &Url) -> bool {
    matches!(expected.scheme(), "http" | "https")
        && expected.scheme() == actual.scheme()
        && expected.origin() == actual.origin()
}

pub fn admit(
    expected: Option<&Url>,
    origin: &Url,
    main_frame: bool,
    body: &Value,
) -> Result<Message> {
    if !main_frame || !expected.is_some_and(|expected| owns(expected, origin)) {
        return Err(Error::Protocol("piHost: not allowed here".into()));
    }
    match body.get("type").and_then(Value::as_str) {
        Some("theme") => Ok(Message::Theme),
        Some("set-theme") => body
            .get("preference")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Protocol("piHost: invalid preference".into()))?
            .parse()
            .map(Message::SetTheme),
        _ => Err(Error::Protocol("piHost: unknown message".into())),
    }
}
