//! Ports `packages/evals/test/comparison.test.ts` plus golden text/output checks
//! recorded from the untouched upstream TypeScript oracle.
#![forbid(unsafe_code)]

use pi_rs_evals::plan::DocumentationVariant;
use pi_rs_evals::report::{
    EvalObservation, EvalOutcome, EvalRunIdentity, format_eval_comparison_report,
    format_eval_comparison_report_with_style, summarize_eval_observations,
};
use serde_json::json;

fn scored(
    variant: DocumentationVariant,
    run_number: u64,
    score: f64,
    metrics: &[(&str, f64)],
) -> EvalObservation {
    let mut observation = EvalObservation {
        eval_set: "tool access".to_owned(),
        case_id: "create".to_owned(),
        variant,
        model: "fixture/model".to_owned(),
        run_number,
        input_tokens: None,
        output_tokens: None,
        cache_read_tokens: None,
        cache_write_tokens: None,
        total_tokens: Some(100.0),
        tool_calls: Some(2.0),
        total_ms: Some(1000.0),
        estimated_cost_usd: Some(0.01),
        outcome: EvalOutcome::Scored,
        score: Some(score),
    };
    for (name, value) in metrics {
        match *name {
            "totalTokens" => observation.total_tokens = Some(*value),
            "toolCalls" => observation.tool_calls = Some(*value),
            "totalMs" => observation.total_ms = Some(*value),
            "estimatedCostUsd" => observation.estimated_cost_usd = Some(*value),
            _ => panic!("unknown metric"),
        }
    }
    observation
}

fn errored(
    variant: DocumentationVariant,
    run_number: u64,
    total_tokens: Option<f64>,
) -> EvalObservation {
    let mut observation = scored(variant, run_number, 0.0, &[]);
    observation.outcome = EvalOutcome::Errored;
    observation.score = None;
    observation.total_tokens = total_tokens;
    observation
}

fn expected_for(runs: &[u64]) -> Vec<EvalRunIdentity> {
    runs.iter()
        .flat_map(|run_number| {
            [
                DocumentationVariant::WithoutDocs,
                DocumentationVariant::WithDocs,
            ]
            .map(|variant| EvalRunIdentity {
                eval_set: "tool access".to_owned(),
                case_id: "create".to_owned(),
                variant,
                model: "fixture/model".to_owned(),
                run_number: *run_number,
            })
        })
        .collect()
}

use DocumentationVariant::{WithDocs as T, WithoutDocs as C};

#[test]
fn computes_paired_lift_and_efficiency_deltas() {
    let observations = vec![
        scored(
            C,
            1,
            0.0,
            &[
                ("totalTokens", 100.0),
                ("toolCalls", 3.0),
                ("totalMs", 1000.0),
            ],
        ),
        scored(
            T,
            1,
            1.0,
            &[
                ("totalTokens", 120.0),
                ("toolCalls", 2.0),
                ("totalMs", 800.0),
            ],
        ),
        scored(C, 2, 1.0, &[("totalTokens", 200.0)]),
        scored(T, 2, 1.0, &[("totalTokens", 180.0)]),
    ];
    let report = summarize_eval_observations("digest", &expected_for(&[1, 2]), &observations);
    let value = serde_json::to_value(&report).expect("serialize report");
    let comparison = &value["comparisons"][0];
    assert_eq!(comparison["evalSet"], "tool access");
    assert_eq!(comparison["totalPairs"], 2);
    assert_eq!(comparison["eligiblePairs"], 2);
    assert_eq!(comparison["blockedPairs"], 0);
    assert_eq!(comparison["controlPassRate"], 0.5);
    assert_eq!(comparison["treatmentPassRate"], json!(1));
    assert_eq!(comparison["lift"], 0.5);
    assert_eq!(
        comparison["totalTokens"],
        json!({"eligiblePairs":2,"controlMean":150,"treatmentMean":150,"meanDelta":0})
    );
    assert_eq!(
        comparison["toolCalls"],
        json!({"eligiblePairs":2,"controlMean":2.5,"treatmentMean":2,"meanDelta":-0.5})
    );
    assert_eq!(
        comparison["totalMs"],
        json!({"eligiblePairs":2,"controlMean":1000,"treatmentMean":900,"meanDelta":-100})
    );
    assert_eq!(
        comparison["estimatedCostUsd"],
        json!({"eligiblePairs":2,"controlMean":0.01,"treatmentMean":0.01,"meanDelta":0})
    );
    assert_eq!(comparison["flags"], json!(["treatment-saturated", "flaky"]));
}

#[test]
fn formatted_report_matches_upstream_golden_text() {
    let observations = vec![
        scored(
            C,
            1,
            0.0,
            &[
                ("totalTokens", 100.0),
                ("toolCalls", 3.0),
                ("totalMs", 1000.0),
            ],
        ),
        scored(
            T,
            1,
            1.0,
            &[
                ("totalTokens", 120.0),
                ("toolCalls", 2.0),
                ("totalMs", 800.0),
            ],
        ),
        scored(C, 2, 1.0, &[("totalTokens", 200.0)]),
        scored(T, 2, 1.0, &[("totalTokens", 180.0)]),
    ];
    let report = summarize_eval_observations("digest", &expected_for(&[1, 2]), &observations);
    let expected = "Documentation Eval Comparisons\n  tool access\n         Pairs  2/2 eligible\n     Pass rate  +50.0 pp (with 100.0%, without 50.0%)\n         Flags  treatment-saturated, flaky\n        Tokens  +0.0 (with 150.0, without 150.0, 2 pairs)\n         Tools  -0.5 (with 2.0, without 2.5, 2 pairs)\n       Latency  -100.0ms (with 900.0ms, without 1000.0ms, 2 pairs)\n     Est. cost  +$0.0000 (with $0.0100, without $0.0100, 2 pairs)\n  Operational totals\n    without_docs: 2 runs, 300 tokens, 5 tools, 2.00s, $0.0200 cost\n    with_docs: 2 runs, 300 tokens, 4 tools, 1.80s, $0.0200 cost";
    assert_eq!(
        format_eval_comparison_report_with_style(&report, false),
        expected
    );
    assert_eq!(
        format_eval_comparison_report_with_style(&report, true),
        format!(
            "\x1b[1mDocumentation Eval Comparisons\x1b[22m{}",
            &expected["Documentation Eval Comparisons".len()..]
        )
    );
}

#[test]
fn fails_closed_for_incomplete_and_errored_pairs_while_retaining_totals() {
    let observations = vec![
        scored(C, 1, 0.0, &[]),
        errored(T, 1, Some(120.0)),
        scored(C, 2, 1.0, &[("totalTokens", 200.0)]),
    ];
    let report = summarize_eval_observations("digest", &expected_for(&[1, 2]), &observations);
    assert_eq!(report.comparisons[0].total_pairs, 2);
    assert_eq!(report.comparisons[0].eligible_pairs, 0);
    assert_eq!(report.comparisons[0].blocked_pairs, 2);
    assert_eq!(report.comparisons[0].lift, None);
    assert_eq!(report.blocked_pairs.len(), 2);
    assert_eq!(report.blocked_pairs[0].run_number, 1);
    assert_eq!(report.blocked_pairs[0].reasons, vec!["with_docs: errored"]);
    assert_eq!(report.blocked_pairs[1].run_number, 2);
    assert_eq!(
        report.blocked_pairs[1].reasons,
        vec!["with_docs: expected 1 observation, found 0"]
    );
    assert_eq!(report.operational_totals[0].total_tokens.available_runs, 2);
    assert_eq!(report.operational_totals[0].total_tokens.total, Some(300.0));
}

#[test]
fn blocks_duplicate_observations_and_keeps_missing_metrics_distinct_from_zero() {
    let mut without_tokens = scored(C, 1, 1.0, &[]);
    without_tokens.total_tokens = None;
    let with_zero = scored(T, 1, 1.0, &[("totalTokens", 0.0)]);
    let report = summarize_eval_observations(
        "digest",
        &expected_for(&[1]),
        &[without_tokens.clone(), without_tokens, with_zero],
    );
    assert_eq!(
        report.blocked_pairs[0].reasons,
        vec!["without_docs: expected 1 observation, found 2"]
    );
    assert_eq!(report.operational_totals[0].total_tokens.available_runs, 0);
    assert_eq!(report.operational_totals[0].total_tokens.total, None);
    assert_eq!(report.operational_totals[1].total_tokens.available_runs, 1);
    assert_eq!(report.operational_totals[1].total_tokens.total, Some(0.0));
}

#[test]
fn formats_blocked_comparisons_and_operational_totals() {
    let report = summarize_eval_observations(
        "digest",
        &expected_for(&[1, 2]),
        &[scored(C, 1, 1.0, &[]), scored(T, 1, 1.0, &[])],
    );
    let formatted = format_eval_comparison_report(&report);
    assert!(formatted.contains("Documentation Eval Comparisons"));
    assert!(formatted.contains("Pass rate  withheld because pairs are blocked"));
    assert!(formatted.contains("without_docs: 1 runs"));
}

#[test]
fn emits_all_five_comparison_flags() {
    // no-lift: equal pass rates; control-saturated and treatment-saturated: both at 1.
    let observations = vec![
        scored(C, 1, 1.0, &[]),
        scored(T, 1, 1.0, &[]),
        scored(C, 2, 1.0, &[]),
        scored(T, 2, 1.0, &[]),
    ];
    let report = summarize_eval_observations("digest", &expected_for(&[1, 2]), &observations);
    assert_eq!(
        report.comparisons[0]
            .flags
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec!["no-lift", "control-saturated", "treatment-saturated"]
    );
    // negative-delta: with_docs scores below without_docs.
    let observations = vec![scored(C, 1, 1.0, &[]), scored(T, 1, 0.0, &[])];
    let report = summarize_eval_observations("digest", &expected_for(&[1]), &observations);
    let flags: Vec<String> = report.comparisons[0]
        .flags
        .iter()
        .map(ToString::to_string)
        .collect();
    assert!(flags.contains(&"negative-delta".to_owned()));
    // flaky: the same control case passes in one run and fails in another.
    let observations = vec![
        scored(C, 1, 0.0, &[]),
        scored(T, 1, 1.0, &[]),
        scored(C, 2, 1.0, &[]),
        scored(T, 2, 1.0, &[]),
    ];
    let report = summarize_eval_observations("digest", &expected_for(&[1, 2]), &observations);
    let flags: Vec<String> = report.comparisons[0]
        .flags
        .iter()
        .map(ToString::to_string)
        .collect();
    assert!(flags.contains(&"flaky".to_owned()), "flags: {flags:?}");
}

#[test]
fn empty_comparisons_format_as_empty_string() {
    let report = summarize_eval_observations("digest", &[], &[]);
    assert_eq!(format_eval_comparison_report(&report), "");
}
