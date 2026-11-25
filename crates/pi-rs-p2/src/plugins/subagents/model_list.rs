//! Model listing and close-match suggestions for the `subagent` tool.

use std::collections::BTreeMap;

/// Minimal model fields read by the listing and fuzzy matcher.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedModel {
    pub provider: String,
    pub id: String,
    pub name: String,
}

impl From<&pi_rs_ai::Model> for ListedModel {
    fn from(model: &pi_rs_ai::Model) -> Self {
        Self {
            provider: model.provider.clone(),
            id: model.id.clone(),
            name: model.name.clone(),
        }
    }
}

const MAX_LISTED: usize = 100;
const MAX_SUGGESTED: usize = 5;

/// Formats the `models` action output, grouping by provider count when more than `MAX_LISTED` match.
pub fn list_models(available: &[ListedModel], query: Option<&str>) -> String {
    let matching: Vec<&ListedModel> = match query {
        None => available.iter().collect(),
        Some(q) => available.iter().filter(|m| matches_query(m, q)).collect(),
    };
    if matching.is_empty() {
        return match query {
            None => "No models with configured credentials.".into(),
            Some(q) => format!("No available model matches {q}."),
        };
    }
    if matching.len() <= MAX_LISTED {
        return matching
            .iter()
            .map(|m| format!("{} ({})", ref_text(m), m.name))
            .collect::<Vec<_>>()
            .join("\n");
    }
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for m in &matching {
        *counts.entry(&m.provider).or_insert(0) += 1;
    }
    let mut lines = Vec::with_capacity(counts.len() + 2);
    lines.push(format!(
        "{} models{}, too many to list. By provider:",
        matching.len(),
        query.map_or_else(String::new, |q| format!(" match {q}"))
    ));
    for (provider, count) in counts {
        lines.push(format!("{provider}: {count}"));
    }
    lines.push(
        "List them with action models and model set to a provider or parts of a model id, e.g. 'anthropic' or 'sonnet 4'."
            .into(),
    );
    lines.join("\n")
}

/// Suggests up to `MAX_SUGGESTED` models whose provider, id, or name contain every word of `text`,
/// prioritizing models from the same provider prefix if `text` contains `/`.
pub fn close_models(available: &[ListedModel], text: &str) -> Vec<String> {
    let provider = text.split_once('/').map(|(p, _)| p);
    let matching: Vec<&ListedModel> = available
        .iter()
        .filter(|m| matches_query(m, text))
        .collect();
    matching
        .iter()
        .filter(|m| Some(m.provider.as_str()) == provider)
        .chain(
            matching
                .iter()
                .filter(|m| Some(m.provider.as_str()) != provider),
        )
        .take(MAX_SUGGESTED)
        .map(|m| ref_text(m))
        .collect()
}

fn matches_query(model: &ListedModel, query: &str) -> bool {
    let wanted = words(query);
    let own = words(&format!("{} {}", ref_text(model), model.name));
    wanted
        .iter()
        .all(|word| own.iter().any(|each| each.starts_with(word)))
}

fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn ref_text(model: &ListedModel) -> String {
    format!("{}/{}", model.provider, model.id)
}
