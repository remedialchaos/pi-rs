//! Ports `packages/evals/test/report.test.ts`: Vitest JSON parsing and session
//! snapshot persistence.
#![forbid(unsafe_code)]

use std::path::Path;

use pi_rs_evals::plan::{DocumentationVariant, EvalTask};
use pi_rs_evals::report::{PI_SESSION_SNAPSHOT_ARTIFACT, read_task_observation};
use serde_json::{Value, json};

fn task() -> EvalTask {
    EvalTask {
        file: "evals/example.docs.eval.ts".to_owned(),
        full_name: "Example workflow > handles the case".to_owned(),
        eval_set: "Example workflow".to_owned(),
        case_id: "handles the case".to_owned(),
        variant: DocumentationVariant::WithoutDocs,
        model: "fixture/model".to_owned(),
        run_number: 1,
    }
}

const SESSION: &str = "{\"type\":\"session\"}\n";

fn write_report(directory: &Path, assertions: &[Value]) -> std::path::PathBuf {
    let report = json!({
        "numFailedTests": assertions.iter().filter(|a| a["status"] == "failed").count(),
        "numPassedTests": assertions.iter().filter(|a| a["status"] == "passed").count(),
        "numPendingTests": 0,
        "numTodoTests": 0,
        "numTotalTests": assertions.len(),
        "startTime": 0,
        "success": assertions.iter().all(|a| a["status"] != "failed"),
        "testResults": [{
            "message": "",
            "name": "/repo/packages/evals/evals/example.docs.eval.ts",
            "status": "passed",
            "assertionResults": assertions,
        }],
    });
    let path = directory.join("vitest.json");
    std::fs::write(
        &path,
        serde_json::to_string(&report).expect("serialize report"),
    )
    .expect("write report");
    path
}

fn assertion(status: &str, meta: Value) -> Value {
    json!({
        "ancestorTitles": ["Example workflow"],
        "fullName": "Example workflow handles the case",
        "status": status,
        "title": "handles the case",
        "failureMessages": [],
        "meta": meta,
    })
}

fn scored_meta(overrides: &[(&str, Value)]) -> Value {
    let mut artifacts = json!({"runId": "run-1", PI_SESSION_SNAPSHOT_ARTIFACT: SESSION});
    let mut usage = json!({
        "provider": "fixture",
        "model": "model",
        "inputTokens": 10,
        "outputTokens": 5,
        "totalTokens": 15,
        "toolCalls": 1,
        "metadata": {"cacheReadTokens": 2, "cacheWriteTokens": 3, "estimatedCostUsd": 0.01},
    });
    let mut errors = json!([]);
    let mut avg_score = json!(0.5);
    for (name, value) in overrides {
        match *name {
            "avgScore" => avg_score = value.clone(),
            "model" => usage["model"] = value.clone(),
            "errors" => errors = value.clone(),
            "artifacts" => artifacts = value.clone(),
            _ => panic!("unknown override"),
        }
    }
    json!({
        "eval": {"avgScore": avg_score, "scores": [{"name": "StructuredOutputJudge", "score": 0.5}], "thresholdFailed": false},
        "harness": {
            "name": "without_docs",
            "run": {
                "output": {"ok": true},
                "session": {"events": [{"type": "message", "role": "user", "content": "prompt"}]},
                "usage": usage,
                "timings": {"totalMs": 1234},
                "artifacts": artifacts,
                "errors": errors,
            },
        },
    })
}

fn read(assertions: &[Value], directory: &Path) -> Value {
    let path = write_report(directory, assertions);
    let observation = read_task_observation(&task(), &path, directory).expect("read observation");
    serde_json::to_value(&observation).expect("serialize observation")
}

#[test]
fn preserves_skipped_pending_and_unscored_outcomes() {
    for status in ["skipped", "todo", "disabled"] {
        let dir = tempfile::tempdir().expect("tempdir");
        let value = read(&[assertion(status, json!({}))], dir.path());
        assert_eq!(value["outcome"], "skipped");
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let value = read(&[assertion("pending", json!({}))], dir.path());
    assert_eq!(value["outcome"], "pending");

    let dir = tempfile::tempdir().expect("tempdir");
    let value = read(
        &[assertion(
            "passed",
            scored_meta(&[("avgScore", Value::Null)]),
        )],
        dir.path(),
    );
    assert_eq!(value["outcome"], "unscored");
    assert_eq!(value["totalTokens"], json!(15));
}

#[test]
fn preserves_metrics_from_a_failed_eval_with_a_partial_harness_run() {
    let dir = tempfile::tempdir().expect("tempdir");
    let value = read(&[assertion("failed", scored_meta(&[]))], dir.path());
    assert_eq!(
        value,
        json!({
            "evalSet": "Example workflow",
            "caseId": "handles the case",
            "variant": "without_docs",
            "model": "fixture/model",
            "runNumber": 1,
            "inputTokens": 10,
            "outputTokens": 5,
            "cacheReadTokens": 2,
            "cacheWriteTokens": 3,
            "totalTokens": 15,
            "toolCalls": 1,
            "totalMs": 1234,
            "estimatedCostUsd": 0.01,
            "outcome": "errored",
        })
    );
    assert_session_persisted(dir.path());
}

#[test]
fn reads_a_scored_harness_run_and_persists_the_session_artifact() {
    let dir = tempfile::tempdir().expect("tempdir");
    let value = read(&[assertion("passed", scored_meta(&[]))], dir.path());
    assert_eq!(value["outcome"], "scored");
    assert_eq!(value["score"], 0.5);
    assert_eq!(value["totalTokens"], json!(15));
    assert_session_persisted(dir.path());
}

fn assert_session_persisted(artifact_directory: &Path) {
    let path = pi_rs_evals::report::session_snapshot_path(artifact_directory, &task());
    let contents = std::fs::read_to_string(&path).expect("session.jsonl exists");
    assert_eq!(contents, SESSION);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let file_mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);
        let directory_mode = std::fs::metadata(path.parent().expect("parent"))
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700);
    }
}

#[test]
fn selects_the_requested_case_when_a_sibling_case_is_skipped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sibling = json!({
        "ancestorTitles": ["Example workflow"],
        "fullName": "Example workflow sibling case",
        "status": "skipped",
        "title": "sibling case",
        "failureMessages": [],
        "meta": {},
    });
    let value = read(
        &[sibling, assertion("passed", scored_meta(&[]))],
        dir.path(),
    );
    assert_eq!(value["outcome"], "scored");
    assert_eq!(value["score"], 0.5);
}

#[test]
fn treats_zero_score_as_scored_data() {
    let dir = tempfile::tempdir().expect("tempdir");
    let value = read(
        &[assertion(
            "passed",
            scored_meta(&[("avgScore", json!(0.0))]),
        )],
        dir.path(),
    );
    assert_eq!(value["outcome"], "scored");
    assert_eq!(value["score"], json!(0));
}

#[test]
fn records_errored_outcomes_for_bad_reports_and_model_mismatch() {
    let dir = tempfile::tempdir().expect("tempdir");
    let value = read(&[assertion("passed", json!({}))], dir.path());
    assert_eq!(value["outcome"], "errored");

    let dir = tempfile::tempdir().expect("tempdir");
    let value = read(
        &[assertion(
            "passed",
            scored_meta(&[("model", json!("other"))]),
        )],
        dir.path(),
    );
    assert_eq!(value["outcome"], "errored");

    let dir = tempfile::tempdir().expect("tempdir");
    let value = read(
        &[assertion(
            "passed",
            scored_meta(&[("errors", json!([{"message":"boom"}]))]),
        )],
        dir.path(),
    );
    assert_eq!(value["outcome"], "errored");
}

#[test]
fn still_scores_a_completed_eval_when_the_session_artifact_is_missing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let value = read(
        &[assertion(
            "passed",
            scored_meta(&[("artifacts", json!({"runId": "run-1"}))]),
        )],
        dir.path(),
    );
    assert_eq!(value["outcome"], "scored");
    assert_eq!(value["score"], 0.5);
}
