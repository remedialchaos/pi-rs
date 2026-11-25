//! The `ports` tool's worker-side logic: the request from its arguments and the reply the
//! agent sees. The tool's extension wiring (`createPortsExtension`'s `Extensions`
//! contribution, and the `locked: "ports"` / `scope: "main"` guard the runtime applies) needs
//! the p2 worker-app seam; this module is the production logic that wiring runs.

use serde_json::{Map, Value, json};

use super::protocol::describe;

/// The guard the runtime applies to the tool, as upstream's P2Tool metadata states it.
pub const LOCKED: &str = "ports";
/// The tool works in the session's main conversation only.
pub const SCOPE_MAIN: bool = true;
/// Every action is idempotent: opening twice returns the same forward, closing twice reports
/// it closed.
pub const REPLAY_SAFE: bool = true;

/// The tool's name.
pub const NAME: &str = "ports";

/// The tool's description, as upstream states it.
pub fn description() -> String {
    [
        "Forward web servers on this device through Radius, so the owner can open them from anywhere.",
        "list: every forward with its addresses and who opened it, and all public endpoints.",
        "open (port; optional name, public): forward a port and return its address. Private by default (owner only, after signing in). public: true also opens it to anyone with the link, without signing in; ask the user before doing that. Opening a port that is already forwarded, by anyone, keeps its address and updates its name and visibility.",
        "close (port): stop forwarding a port, whoever opened it.",
        "Only ports of this device: a session on a remote environment cannot forward its host's ports yet. A session's forwards close when it is archived.",
    ]
    .join("\n")
}

/// The section the tool adds to the session's system prompt when it is the main conversation.
pub fn section() -> &'static str {
    "The ports tool forwards a local web server (a dev server, say) through Radius so the owner can open it from other devices, and lists all forwards and public endpoints. Forwards are private by default: only the owner can open them, after signing in. Never make a port public, by opening it with public: true or switching an existing one, without asking the user first and getting a yes: a public port is reachable by anyone with the link, without signing in."
}

/// The request the tool's arguments make, or the refusal text when they do not.
pub fn ports_request(args: &Value) -> Result<Value, String> {
    let action = args["action"].as_str().unwrap_or_default();
    if action == "list" {
        return Ok(json!({"action": "list"}));
    }
    if args["port"].as_u64().is_none() {
        return Err(format!("{action} needs a port."));
    }
    let port = super::protocol::validate_forward_port(&args["port"]).map_err(|e| e.to_string())?;
    match action {
        "open" => {
            let mut request = Map::new();
            request.insert("action".into(), json!("open"));
            request.insert("port".into(), json!(port));
            if let Some(name) = args.get("name") {
                request.insert("name".into(), name.clone());
            }
            if let Some(public) = args.get("public") {
                request.insert("public".into(), public.clone());
            }
            Ok(Value::Object(request))
        }
        "close" => Ok(json!({"action": "close", "port": port})),
        other => Err(format!("{other} needs a port.")),
    }
}

/// The tool's result for one request and reply: the text the agent reads, the details it
/// carries, and whether it failed.
pub fn ports_reply(args: &Value, result: &Value, own_session: &str) -> Value {
    let action = args["action"].as_str().unwrap_or_default();
    let summary = if action == "open" && !result["forward"].is_null() {
        let forward = &result["forward"];
        let visibility = if forward["public"].as_bool().unwrap_or(false) {
            ", public"
        } else {
            ", private"
        };
        format!(
            "Forwarded port {}{}: {}{}.",
            forward["port"],
            visibility,
            forward["url"].as_str().unwrap_or("address not known yet"),
            forward["publicUrl"]
                .as_str()
                .map(|url| format!(" (public: {url})"))
                .unwrap_or_default(),
        )
    } else if action == "close" {
        if result["closed"].as_bool().unwrap_or(false) {
            format!("Stopped forwarding port {}.", args["port"])
        } else {
            format!("Port {} was not forwarded.", args["port"])
        }
    } else {
        String::new()
    };
    let mut details = Map::new();
    details.insert("action".into(), json!(action));
    if !args["port"].is_null() {
        details.insert("port".into(), args["port"].clone());
    }
    if !result["forward"].is_null() {
        details.insert("forward".into(), result["forward"].clone());
    }
    let text = [summary, describe(result, Some(own_session))]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    json!({
        "output": [{"type": "text", "text": text}],
        "details": Value::Object(details),
    })
}

/// The tool's refusal reply, as upstream's error path returns it.
pub fn ports_error(args: &Value, message: &str) -> Value {
    let action = args["action"].as_str().unwrap_or_default();
    let mut details = Map::new();
    details.insert("action".into(), json!(action));
    if !args["port"].is_null() {
        details.insert("port".into(), args["port"].clone());
    }
    json!({
        "output": [{"type": "text", "text": message}],
        "details": Value::Object(details),
        "isError": true,
    })
}
