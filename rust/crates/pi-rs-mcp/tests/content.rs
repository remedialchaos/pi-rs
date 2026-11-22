//! `packages/mcp/test/content.test.ts`.

use pi_rs_mcp::{CallToolResult, LlmContent, to_llm_content};
use serde_json::json;

#[test]
fn passes_text_and_images_through_and_replaces_other_blocks_with_text() {
    let blocks = vec![
        json!({ "type": "text", "text": "hello", "annotations": { "priority": 1 } }),
        json!({ "type": "image", "data": "aW1n", "mimeType": "image/png", "_meta": { "x": 1 } }),
        json!({ "type": "audio", "data": "YXVk", "mimeType": "audio/wav" }),
        json!({ "type": "resource_link", "uri": "file:///a.txt", "name": "a.txt" }),
        json!({ "type": "resource", "resource": { "uri": "file:///b.txt", "text": "inline" } }),
        json!({ "type": "resource", "resource": { "uri": "file:///c.png", "mimeType": "image/png", "blob": "Yw==" } }),
        json!({ "type": "resource", "resource": { "uri": "file:///d.bin", "blob": "ZA==" } }),
    ];
    let result = CallToolResult {
        content: blocks,
        ..CallToolResult::default()
    };
    assert_eq!(
        to_llm_content(&result),
        vec![
            LlmContent::Text {
                text: "hello".to_string()
            },
            LlmContent::Image {
                data: "aW1n".to_string(),
                mime_type: "image/png".to_string()
            },
            LlmContent::Text {
                text: "[audio audio/wav omitted]".to_string()
            },
            LlmContent::Text {
                text: "a.txt: file:///a.txt".to_string()
            },
            LlmContent::Text {
                text: "inline".to_string()
            },
            LlmContent::Image {
                data: "Yw==".to_string(),
                mime_type: "image/png".to_string()
            },
            LlmContent::Text {
                text: "[binary resource file:///d.bin (unknown type) omitted]".to_string()
            },
        ]
    );
}

#[test]
fn falls_back_to_the_structured_content_as_json_when_there_are_no_blocks() {
    // #10416: structured content may be any JSON value.
    let result = CallToolResult {
        content: Vec::new(),
        structured_content: Some(json!([1, 2])),
        ..CallToolResult::default()
    };
    assert_eq!(
        to_llm_content(&result),
        vec![LlmContent::Text {
            text: "[\n  1,\n  2\n]".to_string()
        }]
    );
    let result = CallToolResult {
        content: Vec::new(),
        structured_content: Some(serde_json::Value::Null),
        ..CallToolResult::default()
    };
    assert_eq!(
        to_llm_content(&result),
        vec![LlmContent::Text {
            text: "null".to_string()
        }]
    );
    let result = CallToolResult {
        content: Vec::new(),
        structured_content: Some(json!({ "n": 1 })),
        ..CallToolResult::default()
    };
    assert_eq!(
        to_llm_content(&result),
        vec![LlmContent::Text {
            text: "{\n  \"n\": 1\n}".to_string()
        }]
    );
    let result = CallToolResult {
        content: vec![json!({ "type": "text", "text": "n=1" })],
        structured_content: Some(json!({ "n": 1 })),
        ..CallToolResult::default()
    };
    assert_eq!(
        to_llm_content(&result),
        vec![LlmContent::Text {
            text: "n=1".to_string()
        }]
    );
}
