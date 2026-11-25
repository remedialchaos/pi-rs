//! `packages/mcp/test/output-limit.test.ts`.

use std::sync::{Arc, Mutex};

use pi_rs_mcp::{LlmContent, MCP_OUTPUT_MAX_BYTES, McpOutputSaver, SaveOutput, limit_mcp_content};
fn image() -> LlmContent {
    LlmContent::Image {
        data: "AAAA".to_string(),
        mime_type: "image/png".to_string(),
    }
}

fn text_of(content: &[LlmContent]) -> String {
    match content.first() {
        Some(LlmContent::Text { text }) => text.clone(),
        _ => panic!("expected a text block first"),
    }
}

/// A saver that records what it got and returns a fixed path.
fn recording_saver(
    saves: Arc<Mutex<Vec<(SaveOutput, String)>>>,
    path: &'static str,
) -> McpOutputSaver {
    Arc::new(move |data, extension| {
        let saves = saves.clone();
        let path = path.to_string();
        let extension = extension.to_string();
        Box::pin(async move {
            saves
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push((data, extension));
            Ok(path)
        })
    })
}

/// A saver that fails with a fixed message.
fn failing_saver(message: &'static str) -> McpOutputSaver {
    let message = message.to_string();
    Arc::new(move |_data, _extension| {
        let message = message.clone();
        Box::pin(async move { Err(message.clone()) })
    })
}

#[tokio::test]
async fn returns_content_within_the_limit_as_it_is_without_saving() {
    let content = vec![
        LlmContent::Text {
            text: "x".repeat(MCP_OUTPUT_MAX_BYTES),
        },
        image(),
    ];
    let saves = Arc::new(Mutex::new(Vec::new()));
    let limited = limit_mcp_content(
        content.clone(),
        Some(recording_saver(saves.clone(), "/tmp/full.txt")),
    )
    .await;
    assert_eq!(limited.content, content);
    assert_eq!(limited.full_output_path, None);
    assert!(
        saves
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty()
    );
}

#[tokio::test]
async fn cuts_the_middle_of_longer_text_saves_the_full_text_and_keeps_images() {
    let full = (0..3000)
        .map(|index| format!("line {}", index + 1))
        .collect::<Vec<_>>()
        .join("\n");
    let saves = Arc::new(Mutex::new(Vec::new()));
    let split = full
        .match_indices('\n')
        .find(|(at, _)| *at >= 100)
        .map(|(at, _)| at)
        .unwrap_or(0);
    let content = vec![
        LlmContent::Text {
            text: full[..split].to_string(),
        },
        image(),
        LlmContent::Text {
            text: full[split + 1..].to_string(),
        },
    ];
    let limited = limit_mcp_content(
        content,
        Some(recording_saver(saves.clone(), "/tmp/full.txt")),
    )
    .await;
    // Text blocks are joined with newlines before the limit applies.
    let recorded = saves
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].0, SaveOutput::Text(full.clone()));
    assert_eq!(recorded[0].1, ".txt");
    assert_eq!(limited.full_output_path.as_deref(), Some("/tmp/full.txt"));
    assert_eq!(limited.content.len(), 2);
    assert_eq!(limited.content[1], image());
    let text = text_of(&limited.content);
    assert!(text.starts_with(&format!(
		"Warning: truncated output (original token count: {})\nTotal output lines: 3000\n\nline 1\nline 2\n",
		(full.len() as f64 / 4.0).ceil() as i64
	)));
    assert!(text.contains(" chars truncated\u{2026}"));
    assert!(text.ends_with("\n\n[Full output: /tmp/full.txt (read it with offset/limit)]"));
    assert!(text.len() < MCP_OUTPUT_MAX_BYTES + 1024);
}

#[tokio::test]
async fn reports_a_failed_save_and_names_no_file_without_a_saver() {
    let content = vec![LlmContent::Text {
        text: "y".repeat(MCP_OUTPUT_MAX_BYTES + 1),
    }];
    let failed = limit_mcp_content(content.clone(), Some(failing_saver("disk full"))).await;
    assert_eq!(failed.full_output_path, None);
    assert!(text_of(&failed.content).ends_with("\n\n[Could not save the full output: disk full]"));

    let unsaved = limit_mcp_content(content, None).await;
    assert_eq!(unsaved.full_output_path, None);
    let text = text_of(&unsaved.content);
    assert!(text.starts_with("Warning: truncated output"));
    assert!(text.contains(" chars truncated\u{2026}"));
    assert!(text.ends_with(&"y".repeat(1024)));
}

#[tokio::test]
async fn cuts_multi_byte_text_only_at_character_boundaries() {
    // After the one-byte prefix, characters start at odd bytes, and both cuts (byte 10240 and
    // byte 40004 - 10240) are even: each lands inside a two-byte character.
    let text = format!("a{}end", "\u{e9}".repeat(20_000));
    let limited = limit_mcp_content(vec![LlmContent::Text { text: text.clone() }], None).await;
    let limited_text = text_of(&limited.content);
    assert!(!limited_text.contains("\u{FFFD}"));
    assert!(limited_text.ends_with("end"));
    // The body between the header's "\n\n" and the truncation marker, then the marker's count.
    let body_start = limited_text.find("\n\n").map(|at| at + 2).unwrap_or(0);
    let marker_start = limited_text[body_start..]
        .find('\u{2026}')
        .map(|at| body_start + at)
        .unwrap_or(body_start);
    let head = &limited_text[body_start..marker_start];
    let after_marker = &limited_text[marker_start + '\u{2026}'.len_utf8()..];
    let digits: String = after_marker
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let removed: usize = digits.parse().unwrap_or(0);
    let tail_start =
        marker_start + '\u{2026}'.len_utf8() + digits.len() + " chars truncated\u{2026}".len();
    let tail = &limited_text[tail_start..];
    assert!(head.len() <= MCP_OUTPUT_MAX_BYTES / 2);
    assert!(tail.len() <= MCP_OUTPUT_MAX_BYTES / 2);
    assert_eq!(
        head.chars().count() + tail.chars().count() + removed,
        text.chars().count()
    );
}
