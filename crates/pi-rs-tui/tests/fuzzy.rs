//! The tests of `packages/tui/test/fuzzy.test.ts`.

use pi_rs_tui::fuzzy::{fuzzy_filter, fuzzy_match};

#[test]
fn empty_query_matches_everything_with_score_0() {
    let result = fuzzy_match("", "anything");
    assert!(result.matches);
    assert_eq!(result.score, 0.0);
}

#[test]
fn query_longer_than_text_does_not_match() {
    let result = fuzzy_match("longquery", "short");
    assert!(!result.matches);
}

#[test]
fn exact_match_has_good_score() {
    let result = fuzzy_match("test", "test");
    assert!(result.matches);
    // Should be negative due to the consecutive bonuses.
    assert!(result.score < 0.0);
}

#[test]
fn characters_must_appear_in_order() {
    let match_in_order = fuzzy_match("abc", "aXbXc");
    assert!(match_in_order.matches);

    let match_out_of_order = fuzzy_match("abc", "cba");
    assert!(!match_out_of_order.matches);
}

#[test]
fn case_insensitive_matching() {
    let result = fuzzy_match("ABC", "abc");
    assert!(result.matches);

    let result2 = fuzzy_match("abc", "ABC");
    assert!(result2.matches);
}

#[test]
fn consecutive_matches_score_better_than_scattered_matches() {
    let consecutive = fuzzy_match("foo", "foobar");
    let scattered = fuzzy_match("foo", "f_o_o_bar");

    assert!(consecutive.matches);
    assert!(scattered.matches);
    assert!(consecutive.score < scattered.score);
}

#[test]
fn word_boundary_matches_score_better() {
    let at_boundary = fuzzy_match("fb", "foo-bar");
    let not_at_boundary = fuzzy_match("fb", "afbx");

    assert!(at_boundary.matches);
    assert!(not_at_boundary.matches);
    assert!(at_boundary.score < not_at_boundary.score);
}

#[test]
fn scores_a_contiguous_occurrence_instead_of_an_earlier_scattered_alignment() {
    // The greedy alignment took o, p, u from "openrouter", so the real "opus" scored worst.
    let opus = fuzzy_match("opus", "openrouter openrouter/anthropic/claude-opus-4.1");
    let sonnet = fuzzy_match("opus", "openrouter openrouter/anthropic/claude-sonnet-4.5");
    let gpt_oss = fuzzy_match("opus", "openrouter openrouter/openai/gpt-oss-120b");

    assert!(opus.score < sonnet.score);
    assert!(opus.score < gpt_oss.score);
}

#[test]
fn uses_the_best_contiguous_occurrence_not_the_first() {
    let embedded_first = fuzzy_match("opus", "xxopus opus");
    let boundary_only = fuzzy_match("opus", "xxxxxx opus");

    assert_eq!(embedded_first.score, boundary_only.score);
}

#[test]
fn matches_swapped_alpha_numeric_tokens() {
    let result = fuzzy_match("codex52", "gpt-5.2-codex");
    assert!(result.matches);
}

#[test]
fn empty_query_returns_all_items_unchanged() {
    let items = ["apple", "banana", "cherry"];
    let result = fuzzy_filter(&items, "", |item: &&str| item.to_string());
    assert_eq!(result, items);
}

#[test]
fn filters_out_non_matching_items() {
    let items = ["apple", "banana", "cherry"];
    let result = fuzzy_filter(&items, "an", |item: &&str| item.to_string());
    assert!(result.contains(&"banana"));
    assert!(!result.contains(&"apple"));
    assert!(!result.contains(&"cherry"));
}

#[test]
fn sorts_results_by_match_quality() {
    let items = ["a_p_p", "app", "application"];
    let result = fuzzy_filter(&items, "app", |item: &&str| item.to_string());

    // "app" should be first (exact consecutive match at start).
    assert_eq!(result[0], "app");
}

#[test]
fn prioritizes_exact_matches_over_longer_prefix_matches() {
    let items = ["clone", "cl"];
    let result = fuzzy_filter(&items, "cl", |item: &&str| item.to_string());

    assert_eq!(result, ["cl", "clone"]);
}

#[test]
fn works_with_a_custom_get_text_function() {
    let items = [("foo", 1), ("bar", 2), ("foobar", 3)];
    let result = fuzzy_filter(&items, "foo", |item: &(&str, i32)| item.0.to_string());

    assert_eq!(result.len(), 2);
    assert!(result.iter().any(|item| item.0 == "foo"));
    assert!(result.iter().any(|item| item.0 == "foobar"));
}

#[test]
fn ranks_a_model_containing_the_query_above_models_matching_a_shared_provider_prefix() {
    // "radius radius/gpt" contains a, s, t, r, a in order, so every radius model matched there.
    let items = [
        "radius radius/gpt-5.4 radius gpt-5.4 GPT 5.4",
        "radius radius/agentic radius agentic Agentic",
        "radius radius/gpt-6-sol radius gpt-6-sol GPT 6 Sol",
        "radius radius/gpt-6-astra radius gpt-6-astra GPT 6 Astra",
    ];
    let result = fuzzy_filter(&items, "astra", |item: &&str| item.to_string());

    assert_eq!(result[0], items[3]);
}

#[test]
fn matches_slash_separated_provider_model_queries_against_reordered_text() {
    let items = [("gpt-5.5", "openai-codex")];
    let result = fuzzy_filter(&items, "openai-codex/gpt-5.5", |item: &(&str, &str)| {
        format!("{} {}", item.0, item.1)
    });

    assert_eq!(result.len(), 1);
    assert_eq!(result[0], ("gpt-5.5", "openai-codex"));
}
