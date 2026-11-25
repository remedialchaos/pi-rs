// The format checks of JSON Schema 2020-12, adapted from TypeBox 1.3.27 (MIT).

use regress::Regex;
use std::sync::LazyLock;

/// Whether the value is a valid ISO 8601 duration.
static DURATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::with_flags(r##"^P((\d+Y(\d+M(\d+D)?)?|\d+M(\d+D)?|\d+D)(T(\d+H(\d+M(\d+S)?)?|\d+M(\d+S)?|\d+S))?|T(\d+H(\d+M(\d+S)?)?|\d+M(\d+S)?|\d+S)|\d+W)$"##, "").expect("the pattern is valid")
});

/// Whether the value has the format.
pub fn is_duration(value: &str) -> bool {
    DURATION.find(value).is_some()
}
