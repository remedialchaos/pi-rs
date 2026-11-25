//! Syntax highlighting: the Rust port of `packages/coding-agent/src/utils/syntax-highlight.ts`.
//!
//! Upstream uses highlight.js v10 with a fixed language set and no auto-detection; `syntect`'s
//! default Sublime syntax set supplies the same common languages. Unknown languages are rejected
//! so callers can fall back to a plain code block, matching upstream's `supportsLanguage`.

use std::sync::OnceLock;

use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::html::{
    ClassStyle, ClassedHTMLGenerator, css_for_theme_with_class_style,
    styled_line_to_highlighted_html,
};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

fn syntax_set() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme_set() -> &'static ThemeSet {
    static SET: OnceLock<ThemeSet> = OnceLock::new();
    SET.get_or_init(ThemeSet::load_defaults)
}

/// Languages the TUI accepts without falling back to an unhighlighted code block.
const LANGUAGES: [&str; 23] = [
    "bash",
    "c",
    "cpp",
    "csharp",
    "css",
    "diff",
    "go",
    "html",
    "java",
    "javascript",
    "json",
    "jsx",
    "kotlin",
    "markdown",
    "php",
    "python",
    "ruby",
    "rust",
    "scala",
    "shell",
    "sql",
    "typescript",
    "yaml",
];

/// Whether `language` is in the supported set (no auto-detection, like upstream).
pub fn supports_language(language: &str) -> bool {
    let language = language.trim().to_ascii_lowercase();
    let aliases = match language.as_str() {
        "js" => "javascript",
        "ts" => "typescript",
        "sh" | "zsh" => "bash",
        "py" => "python",
        "rs" => "rust",
        "yml" => "yaml",
        "md" => "markdown",
        "cs" => "csharp",
        other => other,
    };
    LANGUAGES.contains(&aliases)
}

/// Highlights `code` as `language`, returning an HTML fragment with `hljs-*` classes.
pub fn highlight_to_html(code: &str, language: &str) -> Result<String, String> {
    let language = language.trim().to_ascii_lowercase();
    let aliases = match language.as_str() {
        "js" => "javascript",
        "ts" => "typescript",
        "sh" | "zsh" => "bash",
        "py" => "python",
        "rs" => "rust",
        "yml" => "yaml",
        "md" => "markdown",
        "cs" => "csharp",
        other => other,
    };
    if !LANGUAGES.contains(&aliases) {
        return Err(format!("unsupported language: {language}"));
    }
    let syntax = syntax_set()
        .find_syntax_by_token(aliases)
        .or_else(|| syntax_set().find_syntax_by_first_line(&format!("# {aliases}")))
        .ok_or_else(|| format!("no syntax definition for {aliases}"))?;
    let mut generator = ClassedHTMLGenerator::new_with_class_style(
        syntax,
        syntax_set(),
        ClassStyle::SpacedPrefixed { prefix: "hljs-" },
    );
    for line in LinesWithEndings::from(code) {
        generator
            .parse_html_for_line_which_includes_newline(line)
            .map_err(|error| error.to_string())?;
    }
    Ok(generator.finalize())
}

/// Highlights one line with an inline theme, for the TUI's theme renderer.
pub fn highlight_line_inline(
    line: &str,
    language: &str,
    css_baseline: &str,
) -> Result<String, String> {
    let aliases = match language {
        "js" => "javascript",
        "ts" => "typescript",
        "sh" | "zsh" => "bash",
        "py" => "python",
        "rs" => "rust",
        other => other,
    };
    let syntax = syntax_set()
        .find_syntax_by_token(aliases)
        .ok_or_else(|| format!("no syntax definition for {aliases}"))?;
    let theme = &theme_set().themes[css_baseline];
    let mut highlighter = HighlightLines::new(syntax, theme);
    let regions = highlighter
        .highlight_line(line, syntax_set())
        .map_err(|error| error.to_string())?;
    styled_line_to_highlighted_html(&regions[..], syntect::html::IncludeBackground::No)
        .map_err(|error| error.to_string())
}

/// Prepends the `hljs-*` stylesheet to a document, as upstream's HTML export does.
pub fn stylesheet() -> String {
    let theme = &theme_set().themes["base16-ocean.dark"];
    css_for_theme_with_class_style(theme, ClassStyle::SpacedPrefixed { prefix: "hljs-" })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_common_languages_and_rejects_unknown() {
        assert!(supports_language("typescript"));
        assert!(supports_language("js"));
        assert!(!supports_language("brainfuck"));
    }

    #[test]
    fn highlights_rust_keywords() {
        let html = highlight_to_html("fn main() {}", "rust").unwrap();
        assert!(html.contains("fn"), "{html}");
    }
}
