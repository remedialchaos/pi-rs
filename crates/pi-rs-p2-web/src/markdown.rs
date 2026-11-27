//! `ElanorMarkdown` rendering via `pulldown-cmark`, with p2's safety rules
//! (upstream `Markdown.tsx` + `ElanorMarkdown.tsx`): raw HTML stays text, remote images
//! become links, only web/mail links are clickable, and code blocks are escaped.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// The options upstream uses: GFM (tables, task lists, strikethrough, autolinks).
fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_GFM
        | Options::ENABLE_HEADING_ATTRIBUTES
}

/// Escapes text for HTML (upstream never injects raw HTML).
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Only web and mail addresses are clickable (upstream `safeHref`).
pub fn safe_href(href: &str) -> Option<String> {
    let trimmed = href.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("http:") || lower.starts_with("https:") || lower.starts_with("mailto:") {
        Some(trimmed.to_owned())
    } else {
        None
    }
}

/// A named character reference table for the few names upstream decodes offline.
fn named_entity(name: &str) -> Option<&'static str> {
    Some(match name {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => "\u{a0}",
        "copy" => "©",
        "reg" => "®",
        "trade" => "™",
        "hellip" => "…",
        "mdash" => "—",
        "ndash" => "–",
        "lsquo" => "‘",
        "rsquo" => "’",
        "ldquo" => "“",
        "rdquo" => "”",
        "times" => "×",
        "divide" => "÷",
        "middot" => "·",
        "bull" => "•",
        "larr" => "←",
        "rarr" => "→",
        "euro" => "€",
        "pound" => "£",
        "check" => "✓",
        _ => return None,
    })
}

/// Decodes numeric and named character references in one pass (upstream `entities`).
pub fn entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&'
            && let Some(semi) = text[i..].find(';')
        {
            let reference = &text[i + 1..i + semi];
            let decoded = if let Some(decimal) = reference.strip_prefix('#') {
                decimal
                    .parse::<u32>()
                    .ok()
                    .and_then(char::from_u32)
                    .map(|ch| ch.to_string())
            } else if let Some(hex) = reference
                .strip_prefix("#x")
                .or_else(|| reference.strip_prefix("#X"))
            {
                u32::from_str_radix(hex, 16)
                    .ok()
                    .and_then(char::from_u32)
                    .map(|ch| ch.to_string())
            } else {
                named_entity(reference).map(str::to_owned)
            };
            if let Some(value) = decoded {
                out.push_str(&value);
                i += semi + 1;
                continue;
            }
        }
        let ch = text[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LanguageGrammar {
    Rust,
    TypeScript,
    Python,
    Shell,
    Json,
    HtmlXml,
    Css,
    Sql,
    Go,
    TomlYaml,
    CFamily,
    Diff,
    Generic,
}

impl LanguageGrammar {
    fn from_language(language: Option<&str>) -> Self {
        let Some(raw) = language else {
            return Self::Generic;
        };
        let norm = raw.trim().to_ascii_lowercase();
        match norm.as_str() {
            "rust" | "rs" => Self::Rust,
            "ts" | "tsx" | "typescript" | "js" | "jsx" | "javascript" | "mjs" | "cjs" => {
                Self::TypeScript
            }
            "py" | "python" | "py3" => Self::Python,
            "sh" | "bash" | "zsh" | "shell" | "fish" => Self::Shell,
            "json" | "jsonc" | "jsonl" => Self::Json,
            "html" | "htm" | "xml" | "svg" => Self::HtmlXml,
            "css" | "scss" | "less" => Self::Css,
            "sql" | "sqlite" | "postgres" | "mysql" => Self::Sql,
            "go" | "golang" => Self::Go,
            "toml" | "yaml" | "yml" | "ini" => Self::TomlYaml,
            "c" | "cpp" | "cc" | "cxx" | "h" | "hpp" => Self::CFamily,
            "diff" | "patch" => Self::Diff,
            _ => Self::Generic,
        }
    }

    fn keywords(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &[
                "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else",
                "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match",
                "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct",
                "super", "trait", "true", "type", "unsafe", "use", "where", "while",
            ],
            Self::TypeScript => &[
                "as",
                "async",
                "await",
                "break",
                "case",
                "catch",
                "class",
                "const",
                "continue",
                "default",
                "delete",
                "do",
                "else",
                "enum",
                "export",
                "extends",
                "false",
                "finally",
                "for",
                "from",
                "function",
                "if",
                "implements",
                "import",
                "in",
                "instanceof",
                "interface",
                "let",
                "new",
                "null",
                "of",
                "package",
                "private",
                "protected",
                "public",
                "readonly",
                "return",
                "satisfies",
                "static",
                "super",
                "switch",
                "this",
                "throw",
                "true",
                "try",
                "type",
                "typeof",
                "undefined",
                "var",
                "void",
                "while",
                "yield",
            ],
            Self::Python => &[
                "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class",
                "continue", "def", "del", "elif", "else", "except", "finally", "for", "from",
                "global", "if", "import", "in", "is", "lambda", "nonlocal", "not", "or", "pass",
                "raise", "return", "try", "while", "with", "yield",
            ],
            Self::Shell => &[
                "case", "do", "done", "elif", "else", "esac", "export", "fi", "for", "function",
                "if", "in", "local", "readonly", "return", "select", "then", "until", "while",
            ],
            Self::Json => &["true", "false", "null"],
            Self::HtmlXml => &[],
            Self::Css => &["important", "from", "to", "and", "or", "not", "only"],
            Self::Sql => &[
                "SELECT", "select", "FROM", "from", "WHERE", "where", "INSERT", "insert", "INTO",
                "into", "VALUES", "values", "UPDATE", "update", "SET", "set", "DELETE", "delete",
                "CREATE", "create", "TABLE", "table", "INDEX", "index", "JOIN", "join", "LEFT",
                "left", "RIGHT", "right", "INNER", "inner", "ON", "on", "GROUP", "group", "BY",
                "by", "ORDER", "order", "LIMIT", "limit", "AS", "as", "AND", "and", "OR", "or",
                "NOT", "not", "NULL", "null", "PRIMARY", "primary", "KEY", "key",
            ],
            Self::Go => &[
                "break",
                "case",
                "chan",
                "const",
                "continue",
                "default",
                "defer",
                "else",
                "fallthrough",
                "false",
                "for",
                "func",
                "go",
                "goto",
                "if",
                "import",
                "interface",
                "map",
                "nil",
                "package",
                "range",
                "return",
                "select",
                "struct",
                "switch",
                "true",
                "type",
                "var",
            ],
            Self::TomlYaml => &["true", "false", "null", "yes", "no"],
            Self::CFamily => &[
                "auto",
                "break",
                "case",
                "class",
                "const",
                "continue",
                "default",
                "do",
                "else",
                "enum",
                "extern",
                "false",
                "for",
                "goto",
                "if",
                "inline",
                "namespace",
                "nullptr",
                "private",
                "public",
                "return",
                "sizeof",
                "static",
                "struct",
                "switch",
                "template",
                "this",
                "true",
                "typedef",
                "typename",
                "union",
                "using",
                "virtual",
                "void",
                "volatile",
                "while",
            ],
            Self::Diff => &[],
            Self::Generic => &[
                "fn", "let", "mut", "pub", "if", "else", "for", "while", "return", "const",
                "class", "def", "import", "from", "async", "await", "match", "use", "struct",
                "impl", "true", "false", "null", "None", "Some",
            ],
        }
    }

    fn types(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &[
                "bool", "char", "str", "String", "Vec", "Option", "Result", "Box", "Arc", "Rc",
                "HashMap", "BTreeMap", "usize", "isize", "u8", "u16", "u32", "u64", "i8", "i16",
                "i32", "i64", "f32", "f64", "Some", "None", "Ok", "Err",
            ],
            Self::TypeScript => &[
                "string", "number", "boolean", "unknown", "never", "any", "object", "Promise",
                "Record", "Array", "Map", "Set", "Readonly",
            ],
            Self::Python => &[
                "int", "float", "str", "bool", "list", "dict", "set", "tuple", "Any",
            ],
            Self::Go => &[
                "string", "bool", "int", "int64", "uint64", "byte", "rune", "error", "float64",
            ],
            Self::CFamily => &[
                "int", "char", "long", "short", "unsigned", "signed", "float", "double", "bool",
                "size_t", "uint8_t", "uint32_t", "uint64_t",
            ],
            Self::Sql => &[
                "INTEGER", "TEXT", "BLOB", "REAL", "BOOLEAN", "VARCHAR", "INT",
            ],
            _ => &[],
        }
    }

    fn supports_slash_comments(self) -> bool {
        matches!(
            self,
            Self::Rust
                | Self::TypeScript
                | Self::Go
                | Self::CFamily
                | Self::Css
                | Self::Json
                | Self::Generic
        )
    }

    fn supports_hash_comments(self) -> bool {
        matches!(
            self,
            Self::Python | Self::Shell | Self::TomlYaml | Self::Generic
        )
    }

    fn supports_dash_comments(self) -> bool {
        matches!(self, Self::Sql)
    }
}

fn push_span(out: &mut String, class: &str, text: &str) {
    out.push_str("<span class=\"");
    out.push_str(class);
    out.push_str("\">");
    out.push_str(&escape_html(text));
    out.push_str("</span>");
}

fn highlight_diff(code: &str) -> String {
    let mut out = String::with_capacity(code.len());
    for (idx, line) in code.split('\n').enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        if line.starts_with("+++") || line.starts_with("---") || line.starts_with("@@") {
            push_span(&mut out, "hljs-meta", line);
        } else if line.starts_with('+') {
            push_span(&mut out, "hljs-addition", line);
        } else if line.starts_with('-') {
            push_span(&mut out, "hljs-deletion", line);
        } else {
            out.push_str(&escape_html(line));
        }
    }
    out
}

/// Per-language lexical syntax highlighter for code blocks.
/// Emits escaped HTML only (no raw input reaches the output).
pub fn highlight_code(code: &str, language: Option<&str>) -> String {
    let grammar = LanguageGrammar::from_language(language);
    if grammar == LanguageGrammar::Diff {
        return highlight_diff(code);
    }
    let keywords = grammar.keywords();
    let types = grammar.types();
    let mut out = String::with_capacity(code.len());
    let chars: Vec<char> = code.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let rest: String = chars[i..].iter().collect();

        if grammar == LanguageGrammar::HtmlXml && rest.starts_with("<!--") {
            let end = rest.find("-->").map_or(rest.len(), |pos| pos + 3);
            push_span(&mut out, "hljs-comment", &rest[..end]);
            i += rest[..end].chars().count();
            continue;
        }
        if grammar.supports_slash_comments() && rest.starts_with("/*") {
            let end = rest.find("*/").map_or(rest.len(), |pos| pos + 2);
            push_span(&mut out, "hljs-comment", &rest[..end]);
            i += rest[..end].chars().count();
            continue;
        }
        if (grammar.supports_slash_comments() && rest.starts_with("//"))
            || (grammar.supports_hash_comments() && rest.starts_with('#'))
            || (grammar.supports_dash_comments() && rest.starts_with("--"))
        {
            let end = rest.find('\n').unwrap_or(rest.len());
            push_span(&mut out, "hljs-comment", &rest[..end]);
            i += rest[..end].chars().count();
            continue;
        }

        let ch = chars[i];
        if grammar == LanguageGrammar::HtmlXml && ch == '<' {
            let mut end = i + 1;
            if end < chars.len() && chars[end] == '/' {
                end += 1;
            }
            let name_start = end;
            while end < chars.len()
                && (chars[end].is_ascii_alphanumeric() || chars[end] == '-' || chars[end] == ':')
            {
                end += 1;
            }
            if end > name_start {
                let prefix: String = chars[i..name_start].iter().collect();
                let tag_name: String = chars[name_start..end].iter().collect();
                out.push_str(&escape_html(&prefix));
                push_span(&mut out, "hljs-name", &tag_name);
                i = end;
                continue;
            }
        }

        if (grammar == LanguageGrammar::Rust && ch == '#' && chars.get(i + 1) == Some(&'['))
            || (matches!(
                grammar,
                LanguageGrammar::Python | LanguageGrammar::TypeScript
            ) && ch == '@')
        {
            let mut end = i + 1;
            while end < chars.len() && chars[end] != '\n' && (ch != '#' || chars[end] != ']') {
                end += 1;
            }
            if ch == '#' && end < chars.len() && chars[end] == ']' {
                end += 1;
            }
            let text: String = chars[i..end].iter().collect();
            push_span(&mut out, "hljs-meta", &text);
            i = end;
            continue;
        }

        if ch == '"' || ch == '\'' || ch == '`' {
            let quote = ch;
            let mut end = i + 1;
            while end < chars.len() && chars[end] != quote {
                if chars[end] == '\\' && end + 1 < chars.len() {
                    end += 1;
                }
                end += 1;
            }
            let slice_end = if end < chars.len() { end + 1 } else { end };
            let text: String = chars[i..slice_end].iter().collect();
            let is_json_key = grammar == LanguageGrammar::Json
                && quote == '"'
                && chars[slice_end..]
                    .iter()
                    .find(|c| !c.is_whitespace())
                    .is_some_and(|&c| c == ':');
            let class = if is_json_key {
                "hljs-attr"
            } else {
                "hljs-string"
            };
            push_span(&mut out, class, &text);
            i = slice_end;
            continue;
        }

        if ch.is_ascii_digit() {
            let mut end = i;
            while end < chars.len()
                && (chars[end].is_ascii_alphanumeric() || chars[end] == '.' || chars[end] == '_')
            {
                end += 1;
            }
            let text: String = chars[i..end].iter().collect();
            push_span(&mut out, "hljs-number", &text);
            i = end;
            continue;
        }

        if ch.is_alphabetic() || ch == '_' {
            let mut end = i;
            while end < chars.len() && (chars[end].is_alphanumeric() || chars[end] == '_') {
                end += 1;
            }
            let text: String = chars[i..end].iter().collect();
            if keywords.contains(&text.as_str()) {
                push_span(&mut out, "hljs-keyword", &text);
            } else if types.contains(&text.as_str()) {
                push_span(&mut out, "hljs-type", &text);
            } else if grammar == LanguageGrammar::Rust && chars.get(end) == Some(&'!') {
                push_span(&mut out, "hljs-meta", &format!("{text}!"));
                end += 1;
            } else {
                out.push_str(&escape_html(&text));
            }
            i = end;
            continue;
        }

        out.push_str(&escape_html(&ch.to_string()));
        i += 1;
    }
    out
}

/// Renders an HTML-escaped line-by-line unified diff between `before` and `after`.
pub fn render_diff_html(before: &str, after: &str) -> String {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    let n = a.len();
    let m = b.len();
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut out = String::from("<div class=\"diff-view\">");
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push_str("<div class=\"diff-line diff-ctx\"> ");
            out.push_str(&escape_html(a[i]));
            out.push_str("</div>");
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            out.push_str("<div class=\"diff-line diff-del\">-");
            out.push_str(&escape_html(a[i]));
            out.push_str("</div>");
            i += 1;
        } else {
            out.push_str("<div class=\"diff-line diff-add\">+");
            out.push_str(&escape_html(b[j]));
            out.push_str("</div>");
            j += 1;
        }
    }
    while i < n {
        out.push_str("<div class=\"diff-line diff-del\">-");
        out.push_str(&escape_html(a[i]));
        out.push_str("</div>");
        i += 1;
    }
    while j < m {
        out.push_str("<div class=\"diff-line diff-add\">+");
        out.push_str(&escape_html(b[j]));
        out.push_str("</div>");
        j += 1;
    }
    out.push_str("</div>");
    out
}

/// Renders Markdown as safe HTML with Elanor-compatible classes, exactly as upstream's
/// React renderer does structurally (raw HTML escaped, unsafe links as text).
pub fn render_html(markdown: &str) -> String {
    let mut out = String::new();
    out.push_str("<div class=\"elanor-markdown\">");
    let mut in_code_block = false;
    let mut code_language: Option<String> = None;
    let mut code_buffer = String::new();
    let mut in_link = false;
    let mut heading_stack: Vec<&'static str> = vec![];
    let mut list_stack: Vec<&'static str> = vec![];
    for event in Parser::new_ext(markdown, options()) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => out.push_str("<p>"),
                Tag::Heading { level, .. } => {
                    let tag = match level {
                        HeadingLevel::H1 => "h3",
                        HeadingLevel::H2 => "h4",
                        HeadingLevel::H3 => "h5",
                        _ => "h6",
                    };
                    heading_stack.push(tag);
                    out.push_str(&format!("<{tag}>"));
                }
                Tag::BlockQuote(_) => out.push_str("<blockquote class=\"md\">"),
                Tag::CodeBlock(kind) => {
                    in_code_block = true;
                    code_buffer.clear();
                    code_language = match kind {
                        CodeBlockKind::Fenced(lang) => {
                            let first = lang.split_whitespace().next().unwrap_or("");
                            (!first.is_empty()).then(|| first.to_owned())
                        }
                        CodeBlockKind::Indented => None,
                    };
                }
                Tag::List(Some(start)) => {
                    list_stack.push("ol");
                    out.push_str(&format!("<ol start=\"{start}\">"));
                }
                Tag::List(None) => {
                    list_stack.push("ul");
                    out.push_str("<ul>");
                }
                Tag::Item => out.push_str("<li>"),
                Tag::Emphasis => out.push_str("<em>"),
                Tag::Strong => out.push_str("<strong>"),
                Tag::Strikethrough => out.push_str("<del>"),
                Tag::Link {
                    dest_url, title, ..
                } => {
                    if let Some(href) = safe_href(&dest_url) {
                        in_link = true;
                        let title_attr = if title.is_empty() {
                            String::new()
                        } else {
                            format!(" title=\"{}\"", escape_html(&title))
                        };
                        out.push_str(&format!(
                            "<a href=\"{}\" target=\"_blank\" rel=\"noreferrer\"{title_attr}>",
                            escape_html(&href)
                        ));
                    }
                }
                Tag::Image { dest_url, .. } => {
                    // Remote images are not loaded: a link named by the description.
                    if let Some(href) = safe_href(&dest_url) {
                        in_link = true;
                        out.push_str(&format!(
                            "<a href=\"{}\" target=\"_blank\" rel=\"noreferrer\">",
                            escape_html(&href)
                        ));
                    }
                }
                Tag::Table(alignments) => {
                    let classes = alignments
                        .iter()
                        .map(|a| {
                            format!(
                                "align-{}",
                                match a {
                                    pulldown_cmark::Alignment::Left => "left",
                                    pulldown_cmark::Alignment::Center => "center",
                                    pulldown_cmark::Alignment::Right => "right",
                                    pulldown_cmark::Alignment::None => "left",
                                }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    out.push_str(&format!(
                        "<div class=\"table-wrap\"><table class=\"{classes}\">"
                    ));
                }
                Tag::TableHead => out.push_str("<thead><tr>"),
                Tag::TableRow => out.push_str("<tr>"),
                Tag::TableCell => out.push_str("<td>"),
                Tag::FootnoteDefinition(_) => out.push_str("<p>"),
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => out.push_str("</p>"),
                TagEnd::Heading(_) => {
                    let tag = heading_stack.pop().unwrap_or("h6");
                    out.push_str(&format!("</{tag}>"));
                }
                TagEnd::BlockQuote(_) => out.push_str("</blockquote>"),
                TagEnd::CodeBlock => {
                    let language = code_language.take();
                    let class = language
                        .as_deref()
                        .map(|lang| format!(" class=\"language-{}\"", escape_html(lang)))
                        .unwrap_or_default();
                    let lang_attr = language
                        .as_deref()
                        .map(|lang| format!(" data-language=\"{}\"", escape_html(lang)))
                        .unwrap_or_default();
                    out.push_str(&format!(
                        "<pre{lang_attr}><code{class}>{}</code></pre>",
                        highlight_code(&code_buffer, language.as_deref())
                    ));
                    in_code_block = false;
                    code_buffer.clear();
                }
                TagEnd::List(_) => {
                    let tag = list_stack.pop().unwrap_or("ul");
                    out.push_str(&format!("</{tag}>"));
                }
                TagEnd::Item => out.push_str("</li>"),
                TagEnd::Emphasis => out.push_str("</em>"),
                TagEnd::Strong => out.push_str("</strong>"),
                TagEnd::Strikethrough => out.push_str("</del>"),
                TagEnd::Link | TagEnd::Image => {
                    if in_link {
                        out.push_str("</a>");
                        in_link = false;
                    }
                }
                TagEnd::Table => out.push_str("</table></div>"),
                TagEnd::TableHead => out.push_str("</tr></thead><tbody>"),
                TagEnd::TableRow => out.push_str("</tr>"),
                TagEnd::TableCell => out.push_str("</td>"),
                TagEnd::FootnoteDefinition => out.push_str("</p>"),
                _ => {}
            },
            Event::Text(text) => {
                if in_code_block {
                    code_buffer.push_str(&text);
                } else {
                    out.push_str(&escape_html(&entities(&text)));
                }
            }
            Event::Code(text) => out.push_str(&format!("<code>{}</code>", escape_html(&text))),
            Event::Html(text) | Event::InlineHtml(text) => out.push_str(&escape_html(&text)),
            Event::SoftBreak => {
                if in_code_block {
                    code_buffer.push('\n');
                } else {
                    out.push_str("<br />");
                }
            }
            Event::HardBreak => {
                if in_code_block {
                    code_buffer.push('\n');
                } else {
                    out.push_str("<br />");
                }
            }
            Event::TaskListMarker(checked) => {
                out.push_str(if checked {
                    "<span class=\"markdown-checkbox\" data-checked=\"true\" aria-label=\"Done\">✓</span>"
                } else {
                    "<span class=\"markdown-checkbox\" data-checked=\"false\" aria-label=\"Not done\"></span>"
                });
            }
            Event::Rule => out.push_str("<hr />"),
            Event::InlineMath(text) | Event::DisplayMath(text) => out.push_str(&escape_html(&text)),
            Event::FootnoteReference(label) => out.push_str(&escape_html(&label)),
        }
    }
    out.push_str("</div>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_raw_html_and_script() {
        let html = render_html("hello <script>alert(1)</script> & <b>bold</b>");
        assert!(html.contains("&lt;script&gt;"));
        assert!(!html.contains("<script>"));
        assert!(html.contains("&amp;"));
    }

    #[test]
    fn only_safe_links_are_clickable() {
        let html = render_html("[ok](https://example.com) [bad](javascript:alert(1))");
        assert!(html.contains("href=\"https://example.com\""));
        assert!(html.contains("rel=\"noreferrer\""));
        assert!(!html.contains("javascript:"));
        assert_eq!(html.matches("href=").count(), 1);
    }

    #[test]
    fn code_blocks_are_escaped_and_labelled() {
        let html = render_html("```rust\nfn main() { let x = 1; }\n```");
        assert!(html.contains("data-language=\"rust\""));
        assert!(html.contains("hljs-keyword"));
        assert!(!html.contains("<script>"));
    }

    #[test]
    fn headings_start_at_h3() {
        let html = render_html("# Title\n## Sub\n### More");
        assert!(html.contains("<h3>Title</h3>"));
        assert!(html.contains("<h4>Sub</h4>"));
        assert!(html.contains("<h5>More</h5>"));
    }

    #[test]
    fn tables_and_task_lists_render() {
        let html = render_html("| a | b |\n| --- | --- |\n| 1 | 2 |\n\n- [x] done\n- [ ] todo");
        assert!(html.contains("<table"));
        assert!(html.contains("markdown-checkbox"));
        assert!(html.contains("data-checked=\"true\""));
    }

    #[test]
    fn entities_decode_and_images_become_links() {
        assert_eq!(entities("a &amp; b &#38; c &copy;"), "a & b & c ©");
        let html = render_html("![alt](https://img.example/x.png)");
        assert!(html.contains("href=\"https://img.example/x.png\""));
        assert!(!html.contains("<img"));
    }

    #[test]
    fn per_language_highlighting_and_diff_html() {
        let json_hl = highlight_code(r#"{"port": 8080, "ok": true}"#, Some("json"));
        assert!(json_hl.contains("hljs-attr"));
        assert!(json_hl.contains("hljs-number"));
        assert!(json_hl.contains("hljs-keyword"));

        let py_hl = highlight_code("# note\ndef greet(x: str):\n    return x", Some("python"));
        assert!(py_hl.contains("hljs-comment"));
        assert!(py_hl.contains("hljs-keyword"));
        assert!(py_hl.contains("hljs-type"));

        let rs_hl = highlight_code("let x = println!(\"hi\"); // fn", Some("rust"));
        assert!(rs_hl.contains("hljs-meta"));
        assert!(rs_hl.contains("hljs-comment"));

        let diff_hl = highlight_code("-old\n+new\n@@ -1 +1 @@", Some("diff"));
        assert!(diff_hl.contains("hljs-deletion"));
        assert!(diff_hl.contains("hljs-addition"));
        assert!(diff_hl.contains("hljs-meta"));

        let diff_html = render_diff_html("a\nb", "a\nc");
        assert!(diff_html.contains("diff-del"));
        assert!(diff_html.contains("diff-add"));
    }
}
