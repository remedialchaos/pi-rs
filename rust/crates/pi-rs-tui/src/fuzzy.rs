//! Fuzzy matching utilities.
//!
//! Ported from `packages/tui/src/fuzzy.ts`.
//!
//! Matches if all query characters appear in order (not necessarily consecutive).
//! Lower score = better match.

/// The result of a fuzzy match.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FuzzyMatch {
    /// Whether the text matches the query.
    pub matches: bool,
    /// The score of the match. Lower is better.
    pub score: f64,
}

/// Whether a character starts a word, like the `[\s\-_./:]` of the upstream.
fn is_word_boundary(character: char) -> bool {
    character.is_whitespace()
        || character == '\u{feff}'
        || matches!(character, '-' | '_' | '.' | '/' | ':')
}

/// The index of the first occurrence of `needle` at or after `from`.
fn index_of(haystack: &[char], needle: &[char], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(from.min(haystack.len()));
    }
    if needle.len() > haystack.len() {
        return None;
    }
    (from..=haystack.len() - needle.len())
        .find(|index| haystack[*index..*index + needle.len()] == *needle)
}

/// The score of the matched positions.
fn score_positions(text_lower: &[char], positions: &[usize]) -> f64 {
    let mut score = 0.0;
    let mut last_match_index: i64 = -1;
    let mut consecutive_matches: i64 = 0;

    for position in positions {
        let index = *position as i64;
        // Reward consecutive matches
        if last_match_index == index - 1 {
            consecutive_matches += 1;
            score -= consecutive_matches as f64 * 5.0;
        } else {
            consecutive_matches = 0;
            // Penalize gaps
            if last_match_index >= 0 {
                score += (index - last_match_index - 1) as f64 * 2.0;
            }
        }

        // Reward word boundary matches
        if index == 0 || is_word_boundary(text_lower[(index - 1) as usize]) {
            score -= 10.0;
        }

        // Slight penalty for later matches
        score += index as f64 * 0.1;

        last_match_index = index;
    }

    score
}

/// Matches a normalized query against a normalized text.
fn match_query(normalized_query: &[char], text_lower: &[char]) -> FuzzyMatch {
    if normalized_query.is_empty() {
        return FuzzyMatch {
            matches: true,
            score: 0.0,
        };
    }

    if normalized_query.len() > text_lower.len() {
        return FuzzyMatch {
            matches: false,
            score: 0.0,
        };
    }

    // Greedy alignment: each query character at its first occurrence after the previous one.
    let mut positions: Vec<usize> = Vec::with_capacity(normalized_query.len());
    let mut last_match_index: i64 = -1;
    for character in normalized_query {
        let Some(index) = index_of(text_lower, &[*character], (last_match_index + 1) as usize)
        else {
            return FuzzyMatch {
                matches: false,
                score: 0.0,
            };
        };
        positions.push(index);
        last_match_index = index as i64;
    }

    let mut score = score_positions(text_lower, &positions);

    // The greedy alignment can scatter the query over an earlier, unrelated part of the text,
    // e.g. "opus" in "openrouter openrouter/anthropic/claude-opus" takes o, p, u from "openrouter".
    // Also score every contiguous occurrence and keep the best alignment.
    let mut start = index_of(text_lower, normalized_query, 0);
    while let Some(occurrence) = start {
        for (offset, position) in positions.iter_mut().enumerate() {
            *position = occurrence + offset;
        }
        score = score.min(score_positions(text_lower, &positions));
        start = index_of(text_lower, normalized_query, occurrence + 1);
    }

    if normalized_query == text_lower {
        score -= 100.0;
    }

    FuzzyMatch {
        matches: true,
        score,
    }
}

/// The query with the alpha numeric parts swapped, e.g. `codex52` becomes `52codex`.
fn swapped_query(normalized_query: &[char]) -> Vec<char> {
    let letters = normalized_query
        .iter()
        .take_while(|character| character.is_ascii_lowercase())
        .count();
    let digits = normalized_query
        .iter()
        .skip(letters)
        .take_while(|character| character.is_ascii_digit())
        .count();
    if letters > 0 && digits > 0 && letters + digits == normalized_query.len() {
        let mut swapped = normalized_query[letters..].to_vec();
        swapped.extend_from_slice(&normalized_query[..letters]);
        return swapped;
    }

    let leading_digits = normalized_query
        .iter()
        .take_while(|character| character.is_ascii_digit())
        .count();
    let trailing_letters = normalized_query
        .iter()
        .skip(leading_digits)
        .take_while(|character| character.is_ascii_lowercase())
        .count();
    if leading_digits > 0
        && trailing_letters > 0
        && leading_digits + trailing_letters == normalized_query.len()
    {
        let mut swapped = normalized_query[leading_digits..].to_vec();
        swapped.extend_from_slice(&normalized_query[..leading_digits]);
        return swapped;
    }

    Vec::new()
}

/// Scores a candidate against a query. Lower scores are better matches.
pub fn fuzzy_match(query: &str, text: &str) -> FuzzyMatch {
    let query_lower: Vec<char> = query.to_lowercase().chars().collect();
    let text_lower: Vec<char> = text.to_lowercase().chars().collect();

    let primary_match = match_query(&query_lower, &text_lower);
    if primary_match.matches {
        return primary_match;
    }

    let swapped = swapped_query(&query_lower);
    if swapped.is_empty() {
        return primary_match;
    }

    let swapped_match = match_query(&swapped, &text_lower);
    if !swapped_match.matches {
        return primary_match;
    }

    FuzzyMatch {
        matches: true,
        score: swapped_match.score + 5.0,
    }
}

/// Filters and sorts items by fuzzy match quality (best matches first).
///
/// Supports whitespace- and slash-separated tokens: all tokens must match.
pub fn fuzzy_filter<T, F, S>(items: &[T], query: &str, get_text: F) -> Vec<T>
where
    T: Clone,
    F: Fn(&T) -> S,
    S: AsRef<str>,
{
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return items.to_vec();
    }

    let tokens: Vec<&str> = trimmed
        .split(|character: char| character.is_whitespace() || character == '/')
        .filter(|token| !token.is_empty())
        .collect();

    if tokens.is_empty() {
        return items.to_vec();
    }

    let mut results: Vec<(T, f64)> = Vec::new();

    for item in items {
        let text = get_text(item);
        let text = text.as_ref();
        let mut total_score = 0.0;
        let mut all_match = true;

        for token in &tokens {
            let matched = fuzzy_match(token, text);
            if matched.matches {
                total_score += matched.score;
            } else {
                all_match = false;
                break;
            }
        }

        if all_match {
            results.push((item.clone(), total_score));
        }
    }

    results.sort_by(|first, second| first.1.total_cmp(&second.1));
    results.into_iter().map(|(item, _)| item).collect()
}
