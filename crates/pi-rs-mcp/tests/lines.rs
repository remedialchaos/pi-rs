//! `packages/mcp/test/lines.test.ts`.

use std::sync::{Arc, Mutex};

use serde_json::json;

use pi_rs_mcp::{JsonRpcLineReader, JsonRpcLineReaderOptions, JsonRpcMessage, McpErrorEvent};

type ReaderHarness = (
    JsonRpcLineReader,
    Arc<Mutex<Vec<serde_json::Value>>>,
    Arc<Mutex<Vec<String>>>,
);

fn start_reader(max_message_bytes: Option<u64>) -> ReaderHarness {
    let messages: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let reader = JsonRpcLineReader::new(JsonRpcLineReaderOptions {
        max_message_bytes,
        max_stderr_bytes: None,
        on_stderr: None,
        on_message: {
            let messages = messages.clone();
            Arc::new(move |message: &JsonRpcMessage| {
                messages
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(message.to_json());
            })
        },
        on_error: {
            let errors = errors.clone();
            Arc::new(move |error: McpErrorEvent| {
                errors
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(error.message);
            })
        },
    });
    (reader, messages, errors)
}

fn feed(reader: &mut JsonRpcLineReader, bytes: &[u8], chunk_size: usize) {
    for index in (0..bytes.len()).step_by(chunk_size) {
        reader.read_stdout(&bytes[index..(index + chunk_size).min(bytes.len())]);
    }
}

#[test]
fn joins_a_message_split_across_many_chunks_once_when_its_newline_arrives() {
    let (mut reader, messages, errors) = start_reader(None);
    let text = "x".repeat(1024 * 1024);
    let bytes = format!(
        "{}\n",
        json!({ "jsonrpc": "2.0", "id": 1, "result": { "text": text } })
    )
    .into_bytes();
    feed(&mut reader, &bytes, 4096);
    assert!(
        errors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty()
    );
    assert_eq!(
        messages
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone(),
        vec![json!({ "jsonrpc": "2.0", "id": 1, "result": { "text": text } })]
    );
}

#[test]
fn handles_multi_byte_characters_crlf_blank_lines_and_several_messages_per_chunk() {
    let input = [
        json!({ "jsonrpc": "2.0", "id": 1, "result": { "text": "h\u{e9}llo \u{1F30D}" } })
            .to_string(),
        String::new(),
        format!(
            "{}\r",
            json!({ "jsonrpc": "2.0", "method": "notifications/x" })
        ),
        "not json".to_string(),
        json!({ "jsonrpc": "2.0", "id": 2, "result": {} }).to_string(),
        String::new(),
    ]
    .join("\n")
    .into_bytes();
    for chunk_size in 1..=input.len() {
        let (mut reader, messages, errors) = start_reader(None);
        feed(&mut reader, &input, chunk_size);
        reader.end();
        assert_eq!(
            messages
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
            vec![
                json!({ "jsonrpc": "2.0", "id": 1, "result": { "text": "h\u{e9}llo \u{1F30D}" } }),
                json!({ "jsonrpc": "2.0", "method": "notifications/x" }),
                json!({ "jsonrpc": "2.0", "id": 2, "result": {} }),
            ],
            "messages at chunk size {chunk_size}"
        );
        assert_eq!(
            errors
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len(),
            1,
            "errors at chunk size {chunk_size}"
        );
    }
}

#[test]
fn drops_an_oversized_message_and_keeps_reading_the_next_one() {
    let (mut reader, messages, errors) = start_reader(Some(64));
    let next = json!({ "jsonrpc": "2.0", "id": 2, "result": {} }).to_string();
    feed(&mut reader, "a".repeat(50).as_bytes(), 10);
    feed(
        &mut reader,
        format!("{}\n{}\n", "a".repeat(50), next).as_bytes(),
        10,
    );
    assert_eq!(
        errors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .first()
            .map(String::as_str),
        Some("MCP stdio message exceeds 64 bytes")
    );
    assert_eq!(
        messages
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone(),
        vec![json!({ "jsonrpc": "2.0", "id": 2, "result": {} })]
    );
}

#[test]
fn reports_a_message_without_its_newline_at_the_end() {
    let (mut reader, _messages, errors) = start_reader(None);
    feed(&mut reader, b"{\"jsonrpc\":\"2.0\",", 4);
    reader.end();
    assert_eq!(
        errors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone(),
        vec!["MCP stdio server closed with an incomplete JSON-RPC message".to_string()]
    );
}
