// Component fixture built against pi-rs-chord/wit/plugin.wit.
wit_bindgen::generate!({ world: "plugin" });

struct Notes;
impl Guest for Notes {
    fn setup() -> Result<String, String> {
        Ok(r#"[{"name":"notes","version":1,"members":{
            "commands":{"kind":"state","schema":{},"initial":{
                "notes.show":{"title":"Notes","when":"chat && (owner || participant) && !overlay","slash":{"name":"notes"},"keys":"f4"}}},
            "panel":{"kind":"state","schema":{},"initial":null},
            "run":{"kind":"method","args":{},"result":{}}
        }}]"#
            .into())
    }
    fn invoke(object: String, member: String, args: String, _: String) -> Result<String, String> {
        if member != "run" {
            return Err("unexpected method".into());
        }
        let args: serde_json::Value = serde_json::from_str(&args).map_err(|e| e.to_string())?;
        if args["id"] != "notes.show" {
            return Err("unexpected command".into());
        }
        let subject = args["args"].as_str().unwrap_or_default();
        let lines: Vec<_> = (1..=50).map(|i| format!("note row {i:02}")).collect();
        let panel = serde_json::json!({"id":"notes","title":format!("Guest notes {subject}"),"lines":lines});
        chord::plugin::host::publish(&object, "panel", &panel.to_string())?;
        Ok("null".into())
    }
}
export!(Notes);
fn main() {}
