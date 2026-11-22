// The regular expressions shared across all compiled schemas: tool schemas repeat a few patterns many times, and
// a compiled regular expression is large. The cache holds its values weakly and drops an entry once its regular
// expression is collected, so it keeps nothing alive.

use regress::Regex;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, Weak};

/// A compiled ECMAScript regular expression, with the source it was compiled from.
#[derive(Clone, Debug)]
pub struct Pattern {
    source: String,
    regex: Arc<Regex>,
}

impl Pattern {
    /// The source the regular expression was compiled from.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Whether the pattern matches anywhere in `text` (JavaScript's `RegExp.test`).
    pub fn is_match(&self, text: &str) -> bool {
        self.regex.find(text).is_some()
    }
}

impl PartialEq for Pattern {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source
    }
}

impl Eq for Pattern {}

static REGEXES: LazyLock<Mutex<HashMap<String, Weak<Regex>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Compiles `source` as an ECMAScript regular expression, with the `u` flag; a source the `u` flag rejects
/// (some older schemas escape characters `u` rejects) compiles without it.
pub fn compile(source: &str) -> Result<Pattern, regress::Error> {
    let mut cache = REGEXES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(hit) = cache.get(source).and_then(Weak::upgrade) {
        return Ok(Pattern {
            source: source.to_string(),
            regex: hit,
        });
    }
    let compiled = match Regex::with_flags(source, "u") {
        Ok(regex) => regex,
        Err(_) => Regex::new(source)?,
    };
    let regex = Arc::new(compiled);
    cache.insert(source.to_string(), Arc::downgrade(&regex));
    Ok(Pattern {
        source: source.to_string(),
        regex,
    })
}

/// The number of cached regular expressions (for tests).
pub fn count() -> usize {
    let mut cache = REGEXES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.retain(|_, weak| weak.strong_count() > 0);
    cache.len()
}

/// Whether `source` is a valid ECMAScript regular expression with the `u` flag.
pub fn is_valid(source: &str) -> bool {
    Regex::with_flags(source, "u").is_ok()
}
