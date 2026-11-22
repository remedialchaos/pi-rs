//! Evaluation case discovery and task planning (port of `plan.ts`).

use std::collections::HashSet;
use std::fmt::{Display, Formatter};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{EvalError, EvalResult};

/// The two documentation variants, in upstream declaration order.
pub const DOCUMENTATION_VARIANTS: [DocumentationVariant; 2] = [
    DocumentationVariant::WithoutDocs,
    DocumentationVariant::WithDocs,
];

/// Control (`without_docs`) and treatment (`with_docs`) documentation variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentationVariant {
    WithoutDocs,
    WithDocs,
}

impl DocumentationVariant {
    /// Stable wire string used in JSON, paths, and environment variables.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WithoutDocs => "without_docs",
            Self::WithDocs => "with_docs",
        }
    }

    /// The opposite variant.
    pub const fn other(self) -> Self {
        match self {
            Self::WithoutDocs => Self::WithDocs,
            Self::WithDocs => Self::WithoutDocs,
        }
    }
}

impl Display for DocumentationVariant {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A discovered eval case, derived from a Vitest full name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredEvalCase {
    pub file: String,
    #[serde(rename = "fullName")]
    pub full_name: String,
    #[serde(rename = "evalSet")]
    pub eval_set: String,
    #[serde(rename = "caseId")]
    pub case_id: String,
}

/// One isolated (case, variant, model, run) evaluation task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalTask {
    pub file: String,
    #[serde(rename = "fullName")]
    pub full_name: String,
    #[serde(rename = "evalSet")]
    pub eval_set: String,
    #[serde(rename = "caseId")]
    pub case_id: String,
    pub variant: DocumentationVariant,
    pub model: String,
    #[serde(rename = "runNumber")]
    pub run_number: u64,
}

impl EvalTask {
    /// JSON identity used for session snapshot paths and grouping.
    pub fn identity_json(&self) -> String {
        serde_json::json!([
            self.eval_set,
            self.case_id,
            self.variant.as_str(),
            self.model,
            self.run_number
        ])
        .to_string()
    }
}

/// Parse the JSON emitted by `docker.ts --discover` into discovered cases.
///
/// Mirrors `parseDiscoveredCases`: a strict `<eval set> > <case>` split and
/// rejection of duplicate `(evalSet, caseId)` identities.
pub fn parse_discovered_cases(value: &Value) -> EvalResult<Vec<DiscoveredEvalCase>> {
    let items = value
        .as_array()
        .ok_or_else(|| EvalError::message("Discovered eval cases must be an array."))?;
    let mut identities = HashSet::new();
    let mut cases = Vec::with_capacity(items.len());
    for item in items {
        let (name, file) = match (
            item.get("name").and_then(Value::as_str),
            item.get("file").and_then(Value::as_str),
        ) {
            (Some(name), Some(file)) => (name, file),
            _ => return Err(EvalError::message("Discovered eval case is invalid.")),
        };
        let mut parts = name.split(" > ");
        let eval_set = parts.next().unwrap_or("");
        let case_id = parts.next();
        let extra_count = parts.count();
        if eval_set.trim().is_empty()
            || case_id.is_none_or(|value| value.trim().is_empty())
            || extra_count > 0
        {
            return Err(EvalError::message(format!(
                "Documentation eval must use \"<eval set> > <case>\": {name}"
            )));
        }
        let case_id = case_id.unwrap_or("");
        let identity = serde_json::json!([eval_set, case_id]).to_string();
        if !identities.insert(identity) {
            return Err(EvalError::message(format!(
                "Duplicate eval case identity: {name}"
            )));
        }
        cases.push(DiscoveredEvalCase {
            file: file.to_owned(),
            full_name: name.to_owned(),
            eval_set: eval_set.to_owned(),
            case_id: case_id.to_owned(),
        });
    }
    Ok(cases)
}

/// Build the paired task plan. Odd runs are control-then-treatment; even runs
/// are treatment-then-control.
pub fn create_task_plan(
    cases: &[DiscoveredEvalCase],
    model: &str,
    runs_per_variant: u64,
) -> EvalResult<Vec<EvalTask>> {
    if !model.contains('/') || model.starts_with('/') || model.ends_with('/') {
        return Err(EvalError::message(
            "Model identity must contain a provider and model.",
        ));
    }
    if !(1..=9_007_199_254_740_991).contains(&runs_per_variant) {
        return Err(EvalError::message(
            "Runs per variant must be a positive integer.",
        ));
    }
    let mut tasks = Vec::new();
    for eval_case in cases {
        for run_number in 1..=runs_per_variant {
            let variants = if run_number % 2 == 1 {
                [
                    DocumentationVariant::WithoutDocs,
                    DocumentationVariant::WithDocs,
                ]
            } else {
                [
                    DocumentationVariant::WithDocs,
                    DocumentationVariant::WithoutDocs,
                ]
            };
            for variant in variants {
                tasks.push(EvalTask {
                    file: eval_case.file.clone(),
                    full_name: eval_case.full_name.clone(),
                    eval_set: eval_case.eval_set.clone(),
                    case_id: eval_case.case_id.clone(),
                    variant,
                    model: model.to_owned(),
                    run_number,
                });
            }
        }
    }
    Ok(tasks)
}

#[cfg(test)]
mod tests {
    use super::{DOCUMENTATION_VARIANTS, create_task_plan, parse_discovered_cases};
    use serde_json::json;

    #[test]
    fn variants_match_upstream_wire_names() {
        assert_eq!(DOCUMENTATION_VARIANTS[0].as_str(), "without_docs");
        assert_eq!(DOCUMENTATION_VARIANTS[1].as_str(), "with_docs");
    }

    #[test]
    fn parses_ordinary_vitest_names() {
        let cases = parse_discovered_cases(&json!([
            {"name": "Add model > adds the model", "file": "evals/models.docs.eval.ts"}
        ]))
        .expect("valid case");
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].eval_set, "Add model");
        assert_eq!(cases[0].case_id, "adds the model");
        assert_eq!(cases[0].full_name, "Add model > adds the model");
        assert_eq!(cases[0].file, "evals/models.docs.eval.ts");
    }

    #[test]
    fn rejects_bad_shapes_and_duplicates() {
        assert_eq!(
            parse_discovered_cases(&json!({"name": "A > B", "file": "f"}))
                .expect_err("not an array")
                .to_string(),
            "Discovered eval cases must be an array."
        );
        assert_eq!(
            parse_discovered_cases(&json!([{"name": "A > B"}]))
                .expect_err("missing file")
                .to_string(),
            "Discovered eval case is invalid."
        );
        assert!(
            parse_discovered_cases(&json!([{"name": "adds the model", "file": "model.ts"}]))
                .expect_err("no separator")
                .to_string()
                .contains("<eval set> > <case>")
        );
        assert!(
            parse_discovered_cases(&json!([
                {"name": "A > B > C", "file": "f"}
            ]))
            .expect_err("too many separators")
            .to_string()
            .contains("<eval set> > <case>")
        );
        assert!(
            parse_discovered_cases(&json!([
                {"name": "A > B", "file": "f"},
                {"name": "A > B", "file": "g"}
            ]))
            .expect_err("duplicate")
            .to_string()
            .contains("Duplicate eval case identity")
        );
    }

    #[test]
    fn plan_alternates_variants_by_run() {
        let cases = parse_discovered_cases(&json!([
            {"name": "Add model > adds the model", "file": "evals/models.docs.eval.ts"}
        ]))
        .expect("valid case");
        let tasks = create_task_plan(&cases, "fixture/model", 2).expect("plan");
        let observed: Vec<_> = tasks
            .iter()
            .map(|task| (task.variant.as_str(), task.run_number))
            .collect();
        assert_eq!(
            observed,
            vec![
                ("without_docs", 1),
                ("with_docs", 1),
                ("with_docs", 2),
                ("without_docs", 2),
            ]
        );
    }

    #[test]
    fn plan_validates_model_and_runs() {
        let cases = parse_discovered_cases(&json!([
            {"name": "A > B", "file": "f"}
        ]))
        .expect("valid case");
        for model in ["model", "/model", "model/"] {
            assert!(
                create_task_plan(&cases, model, 1)
                    .expect_err("bad model")
                    .to_string()
                    .contains("provider and model")
            );
        }
        assert!(
            create_task_plan(&cases, "fixture/model", 0)
                .expect_err("bad runs")
                .to_string()
                .contains("positive integer")
        );
    }
}
