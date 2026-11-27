use super::*;
pub(super) fn case() -> Case {
    Case::new(
        "extensions",
        "Create and use a tool extension",
        "creates, reloads, and invokes the extension",
    )
}
pub(super) fn input() -> PiCodingAgentInput {
    steps("Configure this running Pi installation with an extension containing a hello tool that takes a name and returns a greeting. Do not create project source. For example, passing Bob should return `Hello, Bob!`.".into(),Some("Use the hello tool to greet Bob. Respond with exactly the tool's greeting and nothing else."))
}
pub(super) fn judge(run: &SimpleHarnessResult<Value>) -> bool {
    run.errors.is_empty() && run.output.as_str()==Some("Hello, Bob!") && run.events.iter().any(|event| {
        let TranscriptEvent::ToolCall{id,name,arguments} = event else {return false};
        name=="hello" && arguments==&serde_json::json!({"name":"Bob"}) && run.events.iter().any(|e| matches!(e, TranscriptEvent::ToolResult{tool_call_id,name,content,error:None} if id==tool_call_id && name=="hello" && content.as_str()==Some("Hello, Bob!")))
    })
}
