//! Ports `packages/evals/test/plan.test.ts`.
#![forbid(unsafe_code)]

use pi_rs_evals::plan::{create_task_plan, parse_discovered_cases};
use serde_json::json;

fn discovered() -> serde_json::Value {
    json!([{"name": "Add model > adds the model", "file": "evals/models.docs.eval.ts"}])
}

#[test]
fn derives_stable_case_identity_from_ordinary_vitest_names() {
    let cases = parse_discovered_cases(&discovered()).expect("parse cases");
    assert_eq!(cases.len(), 1);
    assert_eq!(cases[0].full_name, "Add model > adds the model");
    assert_eq!(cases[0].eval_set, "Add model");
    assert_eq!(cases[0].case_id, "adds the model");
    assert_eq!(cases[0].file, "evals/models.docs.eval.ts");
}

#[test]
fn rejects_ambiguous_and_duplicate_identities() {
    let error = parse_discovered_cases(&json!([{"name": "adds the model", "file": "model.ts"}]))
        .expect_err("ambiguous name");
    assert!(error.to_string().contains("<eval set> > <case>"));
    let value = json!([
        {"name": "Add model > adds the model", "file": "a.ts"},
        {"name": "Add model > adds the model", "file": "b.ts"}
    ]);
    let error = parse_discovered_cases(&value).expect_err("duplicate identity");
    assert!(error.to_string().contains("Duplicate eval case identity"));
}

#[test]
fn creates_one_isolated_task_per_case_variant_model_and_repetition() {
    let cases = parse_discovered_cases(&discovered()).expect("parse cases");
    let tasks = create_task_plan(&cases, "fixture/model", 2).expect("task plan");
    assert_eq!(tasks.len(), 4);
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
fn alternates_variants_across_runs_one_through_five() {
    let cases = parse_discovered_cases(&discovered()).expect("parse cases");
    let tasks = create_task_plan(&cases, "fixture/model", 5).expect("task plan");
    assert_eq!(tasks.len(), 10);
    for run in 1..=5u64 {
        let pair: Vec<_> = tasks
            .iter()
            .filter(|task| task.run_number == run)
            .map(|task| task.variant.as_str())
            .collect();
        let expected = if run % 2 == 1 {
            vec!["without_docs", "with_docs"]
        } else {
            vec!["with_docs", "without_docs"]
        };
        assert_eq!(pair, expected);
    }
}

#[test]
fn rejects_invalid_model_identities_and_repetitions() {
    let cases = parse_discovered_cases(&discovered()).expect("parse cases");
    for model in ["model", "/model", "model/"] {
        assert!(
            create_task_plan(&cases, model, 1)
                .expect_err("invalid model")
                .to_string()
                .contains("provider and model")
        );
    }
    for runs in [0, 9_007_199_254_740_992] {
        assert!(
            create_task_plan(&cases, "fixture/model", runs)
                .expect_err("invalid runs")
                .to_string()
                .contains("positive integer")
        );
    }
}
