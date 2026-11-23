//! A keyword query as an FTS5 MATCH expression, built here and never taken as raw FTS syntax.
//!
//! Every term is required. `"…"` is a phrase; `word*` a prefix; a term with punctuation
//! (`tx.draft`, `next/diff.ts`) becomes the phrase of its words (`"tx draft"`), as the index's tokenizer splits it.

use regex::Regex;
use std::sync::LazyLock;

/// Characters that are part of a word for the index's tokenizer (unicode61): letters, digits, and the rest of a word.
static WORD: LazyLock<Regex> = LazyLock::new(|| {
    // INFALLIBLE: the pattern is a constant.
    Regex::new(r"[\p{L}\p{N}\p{M}_]+").expect("valid static word pattern")
});

/// A query's terms: a quoted phrase or a run of non-space characters.
static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    // INFALLIBLE: the pattern is a constant.
    Regex::new(r#""([^"]*)"|(\S+)"#).expect("valid static token pattern")
});

/// The words of `text` as the tokenizer sees them.
fn words(text: &str) -> Vec<String> {
    WORD.find_iter(text)
        .map(|word| word.as_str().to_owned())
        .collect()
}

/// A quoted FTS5 string (a phrase of `parts`).
fn phrase(parts: &[String]) -> String {
    format!("\"{}\"", parts.join(" ").replace('"', "\"\""))
}

/// The MATCH expression for `query`, or `None` when it has no searchable word.
pub fn fts_query(query: &str) -> Option<String> {
    let mut terms: Vec<String> = Vec::new();
    for captures in TOKEN.captures_iter(query) {
        if let Some(quoted) = captures.get(1) {
            let parts = words(quoted.as_str());
            if !parts.is_empty() {
                terms.push(phrase(&parts));
            }
            continue;
        }
        let Some(bare) = captures.get(2) else {
            continue;
        };
        let bare = bare.as_str();
        let prefix = bare.ends_with('*');
        let stem = bare.strip_suffix('*').unwrap_or(bare);
        let parts = words(stem);
        if parts.is_empty() {
            continue;
        }
        let quoted = phrase(&parts);
        terms.push(if prefix { format!("{quoted}*") } else { quoted });
    }
    (!terms.is_empty()).then(|| terms.join(" AND "))
}

/// The literal terms of `query`, each required: the message search never reads raw FTS syntax.
pub fn literal_terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .filter(|term| !term.is_empty())
        .map(|term| term.replace('"', "\"\""))
        .collect()
}

/// The MATCH expression of `query`'s literal terms.
pub fn literal_query(query: &str) -> Option<String> {
    let terms = literal_terms(query);
    (!terms.is_empty()).then(|| {
        terms
            .iter()
            .map(|term| format!("\"{term}\""))
            .collect::<Vec<_>>()
            .join(" AND ")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_is_a_list_of_required_terms() {
        assert_eq!(fts_query("draft").as_deref(), Some("\"draft\""));
        assert_eq!(
            fts_query("draft tx").as_deref(),
            Some("\"draft\" AND \"tx\"")
        );
        assert_eq!(fts_query("").as_deref(), None);
        assert_eq!(fts_query("  ...  ").as_deref(), None);
    }

    #[test]
    fn a_quoted_term_is_a_phrase_and_a_star_is_a_prefix() {
        assert_eq!(fts_query("\"two words\"").as_deref(), Some("\"two words\""));
        assert_eq!(fts_query("dra*").as_deref(), Some("\"dra\"*"));
    }

    #[test]
    fn a_punctuated_term_becomes_the_phrase_of_its_words() {
        assert_eq!(fts_query("tx.draft").as_deref(), Some("\"tx draft\""));
        assert_eq!(
            fts_query("next/diff.ts").as_deref(),
            Some("\"next diff ts\"")
        );
    }

    #[test]
    fn the_literal_terms_keep_the_punctuation_they_were_written_with() {
        assert_eq!(literal_terms("tx.draft next").len(), 2);
        assert_eq!(literal_query("tx.draft").as_deref(), Some("\"tx.draft\""));
        assert_eq!(literal_query("   ").as_deref(), None);
    }
}
