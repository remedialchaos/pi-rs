//! Harness pure helpers and an offline faux-backed end-to-end eval run.
#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use pi_rs_ai::Models;
use pi_rs_ai::StopReason;
use pi_rs_ai::faux::{FauxOptions, faux_assistant_message, faux_provider, faux_text};
use pi_rs_evals::harness::{
    DOCUMENTATION_EVAL_TOOLS, PiCodingAgentHarness, PiCodingAgentHarnessOptions,
    PiCodingAgentInput, PiCodingAgentModelSelection, apply_isolated_environment,
    create_faux_harness_session, exclude_pi_documentation, resolve_documentation_variant,
    resolve_model_selection, resolve_sandbox_identity, seed_workspace, verify_system_prompt,
};
use pi_rs_evals::plan::DocumentationVariant;
use pi_rs_evals::report::PI_SESSION_SNAPSHOT_ARTIFACT;
use serde_json::json;

#[test]
fn resolves_model_selection_from_explicit_or_trimmed_environment() {
    let explicit = PiCodingAgentModelSelection {
        provider: "anthropic".to_owned(),
        id: "claude-opus-4-6".to_owned(),
    };
    let selected =
        resolve_model_selection(Some(&explicit), Some("openai-codex"), Some("gpt-5.6-sol"))
            .expect("explicit wins");
    assert_eq!(selected, explicit);

    let selected = resolve_model_selection(None, Some(" openai-codex "), Some(" gpt-5.6-sol "))
        .expect("trimmed env");
    assert_eq!(selected.provider, "openai-codex");
    assert_eq!(selected.id, "gpt-5.6-sol");

    for (provider, model) in [
        (None, None),
        (Some("openai-codex"), None),
        (None, Some("gpt-5.6-sol")),
    ] {
        let error = resolve_model_selection(None, provider, model).expect_err("incomplete");
        assert!(
            error
                .to_string()
                .contains("Select a harness model explicitly")
        );
    }
}

#[test]
fn isolates_environment_and_validates_sandbox_identity() {
    let environment = HashMap::from([
        ("PI_EVAL_VARIANT".to_owned(), "with_docs".to_owned()),
        (
            "PI_EVAL_ARTIFACT_DIR".to_owned(),
            "/tmp/artifacts".to_owned(),
        ),
        ("HOME".to_owned(), "/tmp/original".to_owned()),
    ]);
    let isolated = apply_isolated_environment("/tmp/home", "/tmp/agent", &environment);
    assert_eq!(isolated.home, Path::new("/tmp/home"));
    assert_eq!(isolated.agent_dir, Path::new("/tmp/agent"));
    assert_eq!(
        isolated.removed_eval_vars,
        vec![
            "PI_EVAL_ARTIFACT_DIR".to_owned(),
            "PI_EVAL_VARIANT".to_owned()
        ]
    );

    let mut sandbox = HashMap::new();
    sandbox.insert("PI_EVAL_SANDBOX_UID".to_owned(), "65532".to_owned());
    sandbox.insert("PI_EVAL_SANDBOX_GID".to_owned(), "65532".to_owned());
    let identity = resolve_sandbox_identity(&sandbox)
        .expect("valid identity")
        .expect("present");
    assert_eq!((identity.uid, identity.gid), (65532, 65532));
    sandbox.remove("PI_EVAL_SANDBOX_GID");
    assert_eq!(
        resolve_sandbox_identity(&sandbox)
            .expect_err("incomplete")
            .to_string(),
        "Set both PI_EVAL_SANDBOX_UID and PI_EVAL_SANDBOX_GID, or neither."
    );
}

#[test]
fn validates_documentation_variants_and_prompt_transform() {
    assert_eq!(
        resolve_documentation_variant(Some("without_docs")).expect("control"),
        DocumentationVariant::WithoutDocs
    );
    assert_eq!(
        resolve_documentation_variant(Some("with_docs")).expect("treatment"),
        DocumentationVariant::WithDocs
    );
    for value in [None, Some(""), Some("other")] {
        assert!(
            resolve_documentation_variant(value)
                .expect_err("invalid variant")
                .to_string()
                .contains("PI_EVAL_VARIANT")
        );
    }

    let prompt = "preamble\n<rules>\n- rule\n</rules>\n<docs>\nPi documentation (read only)\n</docs>\n<cwd>\n/workspace\n</cwd>";
    let stripped = exclude_pi_documentation(prompt).expect("strip docs");
    assert!(stripped.contains("\n<rules>\n"));
    assert!(stripped.contains("\n<cwd>\n/workspace\n</cwd>"));
    assert!(!stripped.contains("<docs>"));
    assert!(!stripped.contains("Pi documentation"));
    verify_system_prompt(&stripped, Some("without_docs"), Some(false)).expect("control prompt");
    verify_system_prompt(prompt, Some("with_docs"), Some(true)).expect("treatment prompt");
    assert!(
        verify_system_prompt(&stripped, Some("with_docs"), Some(true))
            .expect_err("wrong variant")
            .to_string()
            .contains("does not match")
    );
    assert!(
        exclude_pi_documentation("Instructions")
            .expect_err("no docs")
            .to_string()
            .contains("no Pi documentation section")
    );
}

#[test]
fn seeds_workspace_with_path_guards() {
    let dir = tempfile::tempdir().expect("tempdir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("mkdir workspace");
    let mut files = BTreeMap::new();
    files.insert("notes/a.txt".to_owned(), "hello".to_owned());
    seed_workspace(&workspace, &files).expect("seed");
    assert_eq!(
        std::fs::read_to_string(workspace.join("notes").join("a.txt")).expect("read"),
        "hello"
    );
    files.clear();
    files.insert("/etc/passwd".to_owned(), "bad".to_owned());
    assert!(seed_workspace(&workspace, &files).is_err());
    files.clear();
    files.insert("../escape.txt".to_owned(), "bad".to_owned());
    assert!(seed_workspace(&workspace, &files).is_err());
    assert_eq!(DOCUMENTATION_EVAL_TOOLS.len(), 6);
}

#[tokio::test]
async fn offline_faux_run_captures_transcript_usage_timings_and_session_artifact() {
    let temp = tempfile::tempdir().expect("tempdir");
    let workspace = temp.path().join("workspace");
    let sessions = temp.path().join("sessions");
    std::fs::create_dir_all(&workspace).expect("mkdir workspace");

    let faux = faux_provider(FauxOptions {
        api: Some("faux-eval".to_owned()),
        ..FauxOptions::default()
    });
    let model = faux.get_model(None).expect("faux model").clone();
    let mut answer = faux_assistant_message(vec![faux_text("answer")]);
    answer.stop_reason = StopReason::Stop;
    faux.set_responses([answer.into()]);

    let models = Models::new(pi_rs_ai::ModelsOptions::default());
    models.set_provider(faux.provider.clone());

    let (sdk, model, artifacts) =
        create_faux_harness_session(&workspace, &sessions, model, models).expect("faux session");
    let harness = PiCodingAgentHarness::new(PiCodingAgentHarnessOptions::default());
    let result = harness
        .run(&sdk, &model, &PiCodingAgentInput::from("hello"), &artifacts)
        .await
        .expect("harness run");

    assert_eq!(result.output, json!("answer"));
    assert!(result.events.iter().any(|event| {
        serde_json::to_value(event).expect("event json")["type"] == "message"
            && serde_json::to_value(event).expect("event json")["role"] == "user"
    }));
    assert!(
        result.events.iter().any(|event| {
            serde_json::to_value(event).expect("event json")["content"] == "answer"
        })
    );
    assert_eq!(
        result.usage.provider.as_deref(),
        Some(model.provider.as_str())
    );
    assert_eq!(result.usage.model.as_deref(), Some(model.id.as_str()));
    assert!(result.usage.input_tokens.unwrap_or(0.0) > 0.0);
    assert!(result.usage.total_tokens.unwrap_or(0.0) > 0.0);
    assert!(result.timings.total_ms >= 0.0);
    assert!(result.artifacts.contains_key("runId"));
    let session = result
        .artifacts
        .get(PI_SESSION_SNAPSHOT_ARTIFACT)
        .and_then(|value| value.as_str())
        .expect("session snapshot artifact");
    assert!(
        session.contains("session") || session.contains("id"),
        "snapshot: {session}"
    );
    assert_eq!(faux.state.call_count(), 1);
    sdk.close();
}
