use std::collections::HashMap;
use std::sync::LazyLock;

use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxSet};

use super::Palette;

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);

#[derive(Clone, Copy)]
enum Style {
    Token(&'static str),
    Attribute(&'static str, &'static str),
}

use Style::{Attribute, Token};

static SCOPE_STYLES: LazyLock<Vec<(Scope, Style)>> = LazyLock::new(|| {
    [
        ("markup.bold", Attribute("\x1b[1m", "\x1b[22m")),
        ("markup.italic", Attribute("\x1b[3m", "\x1b[23m")),
        ("markup.underline.link", Attribute("\x1b[4m", "\x1b[24m")),
        ("markup.inserted", Token("toolDiffAdded")),
        ("markup.deleted", Token("toolDiffRemoved")),
        ("meta.interpolation", Token("text")),
        ("comment", Token("syntaxComment")),
        ("keyword.operator", Token("syntaxOperator")),
        ("keyword", Token("syntaxKeyword")),
        ("storage.type", Token("syntaxType")),
        ("storage.modifier", Token("syntaxKeyword")),
        ("constant.character.escape", Token("syntaxString")),
        ("constant.language", Token("syntaxNumber")),
        ("constant.numeric", Token("syntaxNumber")),
        ("string", Token("syntaxString")),
        ("entity.name.function", Token("syntaxFunction")),
        ("entity.name.type", Token("syntaxType")),
        ("entity.name.class", Token("syntaxType")),
        ("entity.name.tag", Token("syntaxKeyword")),
        ("entity.other.attribute-name", Token("syntaxVariable")),
        ("meta.function.parameters", Token("syntaxVariable")),
        ("variable", Token("syntaxVariable")),
        ("support.function", Token("syntaxFunction")),
        ("support", Token("syntaxType")),
        ("punctuation", Token("syntaxPunctuation")),
        ("meta.preprocessor", Token("muted")),
        ("meta.annotation", Token("muted")),
    ]
    .into_iter()
    .filter_map(|(scope, style)| Scope::new(scope).ok().map(|scope| (scope, style)))
    .collect()
});

fn append_span(
    out: &mut String,
    source: &str,
    scope: &ScopeStack,
    cache: &mut HashMap<Scope, Option<Style>>,
    palette: &Palette,
) {
    if source.is_empty() {
        return;
    }
    let style = scope.as_slice().iter().rev().find_map(|actual| {
        *cache.entry(*actual).or_insert_with(|| {
            SCOPE_STYLES
                .iter()
                .find_map(|(prefix, style)| prefix.is_prefix_of(*actual).then_some(*style))
        })
    });
    match style.unwrap_or(Token("text")) {
        Token(token) => palette.append_fg(out, token, source),
        Attribute(open, close) => {
            out.push_str(open);
            out.push_str(source);
            out.push_str(close);
        }
    }
}

fn highlighted(palette: &Palette, code: &str, language: &str) -> Option<Vec<String>> {
    if language.is_empty() {
        return None;
    }
    let syntax = SYNTAXES.find_syntax_by_token(language).or_else(|| {
        let alias = match language {
            "bash" | "shell" | "zsh" => "sh",
            "csharp" => "cs",
            "javascript" => "js",
            "typescript" => "ts",
            "python" => "py",
            "rust" => "rs",
            "yaml" => "yml",
            _ => return None,
        };
        SYNTAXES.find_syntax_by_extension(alias)
    })?;
    let mut cache = HashMap::new();
    let mut state = ParseState::new(syntax);
    let mut scope = ScopeStack::new();
    let mut lines = Vec::new();
    for line in code.split_inclusive('\n') {
        let visible = line.strip_suffix('\n').unwrap_or(line);
        let mut out = String::with_capacity(visible.len());
        let mut start = 0;
        for (offset, op) in state.parse_line(line, &SYNTAXES).ok()? {
            let end = offset.min(visible.len());
            append_span(
                &mut out,
                visible.get(start..end)?,
                &scope,
                &mut cache,
                palette,
            );
            scope.apply(&op).ok()?;
            start = end;
        }
        append_span(&mut out, visible.get(start..)?, &scope, &mut cache, palette);
        lines.push(out);
    }
    if code.is_empty() || code.ends_with('\n') {
        lines.push(String::new());
    }
    Some(lines)
}

pub(super) fn highlight(palette: &Palette, code: &str, language: Option<&str>) -> Vec<String> {
    if let Some(lines) = language.and_then(|lang| highlighted(palette, code, lang)) {
        return lines;
    }
    code.split('\n')
        .map(|line| {
            let mut out = String::with_capacity(line.len());
            palette.append_fg(&mut out, "mdCodeBlock", line);
            out
        })
        .collect()
}
