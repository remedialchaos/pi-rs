//! The command menu's fuzzy scorer (upstream `command-menu/match.ts`).

/// How well `query` matches `text`: `0` when it does not, higher is better.
///
/// Each word of the query must match as a subsequence of the text, starting at a word
/// start (or as a substring there), with bonuses for word starts and contiguous runs.
/// A query without words matches nothing.
pub fn match_score(query: &str, text: &str) -> f64 {
    let terms: Vec<String> = query
        .to_lowercase()
        .split_whitespace()
        .filter(|term| !term.is_empty())
        .map(str::to_owned)
        .collect();
    if terms.is_empty() {
        return 0.0;
    }
    let chars: Vec<char> = text.chars().collect();
    let lower: Vec<char> = chars
        .iter()
        .map(|c| c.to_lowercase().next().unwrap_or(*c))
        .collect();
    let starts: Vec<bool> = chars
        .iter()
        .enumerate()
        .map(|(index, ch)| index == 0 || word_start(chars.get(index.wrapping_sub(1)).copied(), *ch))
        .collect();
    let mut total = 0.0;
    for term in terms {
        let score = term_score(&term.chars().collect::<Vec<_>>(), &lower, &starts);
        if score == 0.0 {
            return 0.0;
        }
        total += score;
    }
    total
}

/// The `items` whose score is positive, best first (ties keep their order).
pub fn rank_by<T, F: Fn(&T) -> f64>(items: &[T], score: F) -> Vec<&T> {
    let mut scored: Vec<(usize, f64, &T)> = items
        .iter()
        .enumerate()
        .map(|(index, item)| (index, score(item), item))
        .filter(|(_, value, _)| *value > 0.0)
        .collect();
    // Stable descending sort: score first, original order for ties.
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    scored.into_iter().map(|(_, _, item)| item).collect()
}

/// Whether `char` starts a word after `before` (`\p{L}`/`\p{N}` and casing rules).
fn word_start(before: Option<char>, ch: char) -> bool {
    let Some(before) = before else {
        return ch.is_alphanumeric();
    };
    if !before.is_alphanumeric() {
        return ch.is_alphanumeric();
    }
    ch.is_uppercase() && before.is_lowercase()
}

const MATCH: f64 = 1.0;
const TEXT_START: f64 = 8.0;
const WORD_START: f64 = 6.0;
const CONTIGUOUS: f64 = 4.0;
const GAP: f64 = 0.5;
const LEAD: f64 = 0.05;

/// The best alignment of `term` in `text` (both lower case, by character), by dynamic
/// programming in O(term × text). Its first character must sit on a word start, unless
/// the whole term is a substring there.
fn term_score(term: &[char], text: &[char], starts: &[bool]) -> f64 {
    let mut previous: Vec<f64> = vec![];
    for (i, &tch) in term.iter().enumerate() {
        let mut current = vec![f64::NEG_INFINITY; text.len()];
        let mut best_jump = f64::NEG_INFINITY;
        for (j, &ch) in text.iter().enumerate() {
            if i > 0 && j > 0 {
                best_jump = best_jump.max(previous[j - 1] + GAP * (j - 1) as f64);
            }
            if ch != tch {
                continue;
            }
            let bonus = MATCH
                + if j == 0 {
                    TEXT_START
                } else if starts[j] {
                    WORD_START
                } else {
                    0.0
                };
            if i == 0 {
                if starts[j] || substring_at(term, text, j) {
                    current[j] = bonus - LEAD * j as f64;
                }
                continue;
            }
            let contiguous = if j > 0 {
                previous[j - 1] + CONTIGUOUS
            } else {
                f64::NEG_INFINITY
            };
            current[j] = bonus + contiguous.max(best_jump - GAP * (j as f64 - 1.0));
        }
        previous = current;
    }
    let best = previous.into_iter().fold(f64::NEG_INFINITY, f64::max);
    if best.is_finite() { best } else { 0.0 }
}

fn substring_at(term: &[char], text: &[char], at: usize) -> bool {
    term.iter()
        .enumerate()
        .all(|(index, ch)| text.get(at + index) == Some(ch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_matches_nothing() {
        assert_eq!(match_score("", "anything"), 0.0);
        assert_eq!(match_score("   ", "anything"), 0.0);
    }

    #[test]
    fn word_start_bonuses_score_higher_than_mid_word() {
        let start = match_score("cl", "Clear session");
        let middle = match_score("ce", "Archive session");
        assert!(start > 0.0);
        assert!(middle == 0.0 || start > middle);
    }

    #[test]
    fn contiguous_query_scores_and_non_matches_are_zero() {
        assert!(match_score("open", "Open terminal") > 0.0);
        assert!(match_score("op", "open file") > 0.0);
        assert_eq!(match_score("xyz", "Open terminal"), 0.0);
    }

    #[test]
    fn multiple_terms_all_must_match() {
        assert!(match_score("new session", "New session") > 0.0);
        assert_eq!(match_score("new missing", "New session"), 0.0);
    }

    #[test]
    fn rank_by_keeps_order_for_ties_and_drops_zero() {
        let items = vec!["a", "b", "c"];
        let ranked = rank_by(&items, |item| match *item {
            "a" => 2.0,
            "b" => 2.0,
            _ => 0.0,
        });
        assert_eq!(ranked, vec![&"a", &"b"]);
    }
}
