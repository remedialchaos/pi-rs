//! First-party structured submission tool used only by the documentation audit.
wit_bindgen::generate!({ world: "extension", path: "crates/pi-rs-coding-agent/wit" });

struct Audit;
const NAME: &str = "submit_documentation_audit";
impl Guest for Audit {
    fn init() -> Result<(), String> {
        pi::extension::host::register_tool(
            NAME,
            "Submit the final documentation audit verdict after investigating the page and implementation.",
            r#"{"type":"object","properties":{"verdict":{"type":"string","enum":["match","mismatch","inconclusive"]},"explanation":{"type":"string","minLength":1,"maxLength":2000}},"required":["verdict","explanation"],"additionalProperties":false}"#,
        )
    }
    fn on_tool_call(name: String, _: String) -> Result<Option<ToolCallPatch>, String> {
        Ok((name == NAME).then_some(ToolCallPatch {
            block: None,
            reason: None,
            input_json: None,
            terminate: Some(true),
        }))
    }
    fn on_before_headers(_: String) -> Result<Option<HeadersPatch>, String> {
        Ok(None)
    }
    fn execute_tool(name: String, _: String) -> Result<ToolOutcome, String> {
        if name != NAME {
            return Err("unknown audit tool".into());
        }
        Ok(ToolOutcome {
            ok: true,
            payload_json:
                r#"{"content":[{"type":"text","text":"Documentation audit submitted."}]}"#.into(),
            error: None,
        })
    }
    fn run_command(_: String, _: String) -> Result<(), String> {
        Err("no commands".into())
    }
}
export!(Audit);
fn main() {}
