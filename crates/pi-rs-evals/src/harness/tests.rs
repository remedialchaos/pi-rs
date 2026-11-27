use super::{native::finish, *};
use serde_json::json;

#[test]
fn failed_runs_keep_transcript_and_errors_in_partial_run() -> Result<(), Box<dyn std::error::Error>>
{
    let partial = SimpleHarnessResult {
        output: Value::Null,
        events: vec![TranscriptEvent::Message {
            role: TranscriptRole::User,
            content: json!("hello"),
        }],
        usage: UsageSummary {
            provider: Some("test".into()),
            model: Some("fixture".into()),
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            tool_calls: None,
            metadata: Map::new(),
        },
        timings: TimingSummary { total_ms: 0.0 },
        artifacts: BTreeMap::new(),
        metadata: Map::new(),
        errors: Vec::new(),
    };
    let failure = finish(
        partial,
        Instant::now(),
        vec!["provider failed".into()],
        Ok("session transcript".into()),
    )
    .err()
    .ok_or("missing file must fail")?;
    let run = failure.partial_run.ok_or("missing partial run")?;
    assert_eq!(run.events.len(), 1);
    assert_eq!(run.errors.len(), 1);
    assert_eq!(
        run.artifacts[PI_SESSION_SNAPSHOT_ARTIFACT],
        json!("session transcript")
    );
    Ok(())
}

#[test]
fn unexpected_stop_preserves_the_assistant_error() -> Result<(), Box<dyn std::error::Error>> {
    let mut answer = pi_rs_ai::faux::faux_assistant_message(Vec::new());
    answer.stop_reason = StopReason::Error;
    answer.error_message = Some("Provider rejected the prompt".into());
    let error = assistant_response(&[Message::Assistant(answer)])
        .err()
        .ok_or("unexpected stop succeeded")?;
    assert_eq!(error.to_string(), "Provider rejected the prompt");
    Ok(())
}
#[test]
fn missing_session_file_is_reported_before_cleanup() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let result = snapshot(Some(&temp.path().join("missing.jsonl")));
    let error = result.err().ok_or("missing session silently accepted")?;
    assert_eq!(error.to_string(), "Pi eval produced no session file.");
    Ok(())
}
