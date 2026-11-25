//! Web URL helpers, port of `packages/coding-agent/src/utils/web-url.ts`.
//!
//! `web_url` only accepts absolute `http:`/`https:` URLs, because the shared
//! launcher can also open files and custom protocols while login must only open
//! web URLs. Normalization through `url::Url` percent-encodes control characters
//! that could otherwise terminate a terminal hyperlink early.

use url::Url;

/// The normalized `http:` or `https:` URL that `value` names, or `None`.
pub fn web_url(value: &str) -> Option<String> {
    let url = Url::parse(value).ok()?;
    match url.scheme() {
        "https" | "http" => Some(url.as_str().to_string()),
        _ => None,
    }
}

/// Replace runs of control characters (U+0000-U+001F, U+007F-U+009F) with a
/// single space, mirroring `replaceControlCharacters` from the TUI package.
pub fn replace_control_characters(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut in_control_run = false;
    for c in value.chars() {
        let is_control = c <= '\u{001f}' || ('\u{007f}'..='\u{009f}').contains(&c);
        if is_control {
            if !in_control_run {
                result.push(' ');
                in_control_run = true;
            }
        } else {
            result.push(c);
            in_control_run = false;
        }
    }
    result
}

/// `value` normalized as a web URL when possible, otherwise with control
/// characters replaced so an untrusted string cannot emit terminal escapes.
pub fn display_url(value: &str) -> String {
    web_url(value).unwrap_or_else(|| replace_control_characters(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_https_urls() {
        assert_eq!(
            web_url("HTTPS://Example.COM/path"),
            Some("https://example.com/path".to_string())
        );
        assert_eq!(web_url("http://x.test"), Some("http://x.test/".to_string()));
    }

    #[test]
    fn rejects_non_web_urls() {
        assert_eq!(web_url("mailto:user@example.com"), None);
        assert_eq!(web_url("file:///tmp/x"), None);
        assert_eq!(web_url("file:///etc/passwd"), None);
        assert_eq!(web_url("javascript:alert(1)"), None);
        assert_eq!(web_url("JAVASCRIPT:alert(1)"), None);
        assert_eq!(web_url("data:text/html,<script>"), None);
        assert_eq!(web_url("vbscript:msgbox(1)"), None);
        assert_eq!(web_url("not a url"), None);
        assert_eq!(web_url("/relative/path"), None);
    }

    #[test]
    fn display_url_replaces_control_runs() {
        assert_eq!(display_url("ab\u{0001}\u{0002}cd\u{009f}ef"), "ab cd ef");
        assert_eq!(
            display_url("https://example.com/a"),
            "https://example.com/a"
        );
        assert_eq!(display_url("file:///tmp/a\u{0001}b"), "file:///tmp/a b");
        let sanitized = display_url("https://example.com/auth\x1b[31m\r\n\x00\u{009b}");
        assert!(!sanitized.chars().any(|c| c.is_control()));
    }
}
