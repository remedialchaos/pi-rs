//! `packages/mcp/test/model-content.test.ts`.

use std::sync::{Arc, Mutex};

use pi_rs_mcp::{CallToolResult, LlmContent, McpOutputSaver, SaveOutput, to_mcp_model_content};
use serde_json::json;

fn link() -> serde_json::Value {
    json!({
        "type": "resource_link",
        "uri": "docs://guide",
        "name": "guide",
        "title": "The Guide",
        "mimeType": "text/markdown",
        "size": 2048,
        "description": "How to use it",
    })
}

fn pdf() -> serde_json::Value {
    json!({
        "type": "resource",
        "resource": { "uri": "file:///r/report.pdf", "mimeType": "application/pdf", "blob": "JVBERg==" },
    })
}

fn result(blocks: Vec<serde_json::Value>) -> CallToolResult {
    CallToolResult {
        content: blocks,
        ..CallToolResult::default()
    }
}

fn recording_saver(saves: Arc<Mutex<Vec<(SaveOutput, String)>>>) -> McpOutputSaver {
    Arc::new(move |data, extension| {
        let saves = saves.clone();
        let extension = extension.to_string();
        Box::pin(async move {
            let path = format!("/tmp/saved{extension}");
            saves
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push((data, extension));
            Ok(path)
        })
    })
}

#[tokio::test]
async fn names_the_read_tool_in_resource_links_when_there_is_one() {
    let content = to_mcp_model_content(
        "docs",
        &result(vec![link()]),
        pi_rs_mcp::McpModelContentOptions {
            read_resource_tool: Some("read_mcp_resource".to_string()),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(
		content,
		vec![LlmContent::Text {
			text: "[Resource docs://guide \"The Guide\" (text/markdown, 2.0KB): How to use it. Read it with read_mcp_resource (server \"docs\")]"
				.to_string()
		}]
	);
    let content = to_mcp_model_content(
        "docs",
        &result(vec![
            json!({ "type": "resource_link", "uri": "file:///a", "name": "a" }),
        ]),
        Default::default(),
    )
    .await;
    assert_eq!(
        content,
        vec![LlmContent::Text {
            text: "[Resource file:///a \"a\"]".to_string()
        }]
    );
}

#[tokio::test]
async fn saves_binary_resources_shows_text_blobs_as_text_and_keeps_images() {
    let saves = Arc::new(Mutex::new(Vec::new()));
    let saver = recording_saver(saves.clone());
    let json_blob = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(br#"{"a":1}"#)
    };
    let content = vec![
        pdf(),
        json!({
            "type": "resource",
            "resource": { "uri": "docs://data", "mimeType": "application/json; charset=utf-8", "blob": json_blob },
        }),
        json!({ "type": "resource", "resource": { "uri": "docs://logo", "mimeType": "image/png", "blob": "AAAA" } }),
        json!({ "type": "resource", "resource": { "uri": "docs://raw", "blob": "AAAA" } }),
    ];
    let out = to_mcp_model_content(
        "docs",
        &result(content),
        pi_rs_mcp::McpModelContentOptions {
            save_output: Some(saver),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(
		out,
		vec![
			LlmContent::Text { text: "[Binary resource file:///r/report.pdf (application/pdf, 4B) saved to /tmp/saved.pdf]".to_string() },
			LlmContent::Text { text: "{\"a\":1}".to_string() },
			LlmContent::Image { data: "AAAA".to_string(), mime_type: "image/png".to_string() },
			LlmContent::Text { text: "[Binary resource docs://raw (unknown type, 3B) saved to /tmp/saved.bin]".to_string() },
		]
	);
    let recorded = saves
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    assert_eq!(
        recorded,
        vec![
            (SaveOutput::Bytes(b"%PDF".to_vec()), ".pdf".to_string()),
            (SaveOutput::Bytes(vec![0, 0, 0]), ".bin".to_string()),
        ]
    );
}

#[tokio::test]
async fn leaves_binary_resources_out_without_a_saver_and_says_why_when_saving_fails() {
    let content = to_mcp_model_content("docs", &result(vec![pdf()]), Default::default()).await;
    assert_eq!(
        content,
        vec![LlmContent::Text {
            text: "[binary resource file:///r/report.pdf (application/pdf) omitted]".to_string()
        }]
    );
    let failing: McpOutputSaver =
        Arc::new(|_data, _extension| Box::pin(async { Err("disk full".to_string()) }));
    let content = to_mcp_model_content(
        "docs",
        &result(vec![pdf()]),
        pi_rs_mcp::McpModelContentOptions {
            save_output: Some(failing),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(
		content,
		vec![LlmContent::Text {
			text: "[Binary resource file:///r/report.pdf (application/pdf, 4B) could not be saved: disk full]"
				.to_string()
		}]
	);
}

#[tokio::test]
async fn shows_structured_content_as_json_when_there_are_no_content_blocks() {
    let result = CallToolResult {
        content: Vec::new(),
        structured_content: Some(json!([{ "id": 1 }])),
        ..CallToolResult::default()
    };
    let content = to_mcp_model_content("docs", &result, Default::default()).await;
    assert_eq!(
        content,
        vec![LlmContent::Text {
            text: "[\n  {\n    \"id\": 1\n  }\n]".to_string()
        }]
    );
}
