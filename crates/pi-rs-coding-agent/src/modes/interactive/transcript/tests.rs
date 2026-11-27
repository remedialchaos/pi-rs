use std::error::Error;
use std::path::Path;
use std::sync::RwLock;

use pi_rs_ai::{
    AssistantMessageEvent,
    faux::{faux_assistant_message, faux_text, faux_thinking, faux_tool_call},
};
use pi_rs_tui::{strip_terminal_sequences, visible_width};
use serde_json::{json, to_value};

use super::*;

fn dark() -> Result<Theme, Box<dyn Error>> {
    Ok(Theme::load(Path::new("."), Some("dark"))?)
}

fn plain(transcript: &mut Transcript, width: usize) -> String {
    transcript
        .render(width)
        .iter()
        .map(|line| strip_terminal_sequences(line).trim_end().to_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

fn core(transcript: &mut Transcript, event: AgentEvent) {
    transcript.event(&AgentSessionEvent::Core(event));
}

#[test]
fn assistant_and_tool_update_their_rows_in_place() -> Result<(), Box<dyn Error>> {
    let mut transcript = Transcript::new(dark()?);
    let user = AgentMessage::user("Read **notes**");
    core(
        &mut transcript,
        AgentEvent::MessageStart {
            message: user.clone(),
        },
    );
    core(&mut transcript, AgentEvent::MessageEnd { message: user });
    let args = Map::from_iter([("path".to_owned(), json!("notes.txt"))]);
    let partial = Arc::new(RwLock::new(faux_assistant_message(vec![
        faux_thinking("Plan"),
        faux_text("Working"),
        faux_tool_call("read", args.clone(), Some("read-1".into())),
    ])));
    core(
        &mut transcript,
        AgentEvent::MessageUpdate {
            message: partial.clone(),
            assistant_message_event: AssistantMessageEvent::TextDelta {
                content_index: 1,
                delta: "Working".into(),
                partial,
            },
        },
    );
    core(
        &mut transcript,
        AgentEvent::ToolExecutionStart {
            tool_call_id: "read-1".into(),
            tool_name: "read".into(),
            args: args.clone(),
        },
    );
    core(
        &mut transcript,
        AgentEvent::ToolExecutionUpdate {
            tool_call_id: "read-1".into(),
            tool_name: "read".into(),
            args: args.clone(),
            partial_result: AgentToolResult::text("still streaming"),
        },
    );
    let frame = plain(&mut transcript, 70);
    assert_eq!(frame.matches("Read notes").count(), 1);
    assert_eq!(frame.matches("read notes.txt").count(), 1);
    assert!(
        frame.contains("Working") && frame.contains("Plan") && frame.contains("still streaming"),
        "{frame}"
    );
    assert!(!transcript.toggle_thinking());
    let frame = plain(&mut transcript, 70);
    assert!(frame.contains("Thinking...") && !frame.contains("Plan"));
    let mut assistant = faux_assistant_message(vec![
        faux_text("Done"),
        faux_text("."),
        faux_tool_call("read", args, Some("read-1".into())),
    ]);
    assistant.stop_reason = StopReason::ToolUse;
    core(
        &mut transcript,
        AgentEvent::MessageEnd {
            message: assistant.into(),
        },
    );
    core(
        &mut transcript,
        AgentEvent::ToolExecutionEnd {
            tool_call_id: "read-1".into(),
            tool_name: "read".into(),
            result: Arc::new(AgentToolResult::text("actual contents")),
            is_error: false,
            duration_ms: Some(222),
        },
    );
    assert_eq!(transcript.last_assistant_text(), Some("Done."));
    let frame = plain(&mut transcript, 70);
    assert_eq!(frame.matches("read notes.txt").count(), 1);
    assert!(!frame.contains("Working") && !frame.contains("still streaming"));
    assert!(frame.contains("actual contents"));
    Ok(())
}

fn persisted(
    id: &str,
    message: impl Into<AgentMessage>,
) -> Result<SessionEntry, serde_json::Error> {
    Ok(SessionEntry::Message {
        id: id.into(),
        parent_id: None,
        timestamp: "2025-01-01T00:00:00Z".into(),
        message: to_value(message.into())?,
    })
}

#[test]
fn restore_distinguishes_custom_skills_summaries_and_shell_results() -> Result<(), Box<dyn Error>> {
    let mut transcript = Transcript::new(dark()?);
    transcript.notice("old session".into(), "muted");
    let entries = vec![
        persisted(
            "skill",
            AgentMessage::user(
                "<skill name=\"lint\" location=\"/tmp/skill.md\">\nHidden skill body\n</skill>\n\nPlease lint",
            ),
        )?,
        serde_json::from_value(
            json!({ "type":"custom_message", "id":"visible", "timestamp":"t", "customType":"extension", "content":[{"type":"text","text":"Visible card"}], "display":true }),
        )?,
        serde_json::from_value(
            json!({ "type":"custom_message", "id":"hidden", "timestamp":"t", "customType":"hidden", "content":"Invisible card", "display":false }),
        )?,
        serde_json::from_value(
            json!({ "type":"branch_summary", "id":"branch", "timestamp":"t", "fromId":"skill", "summary":"Retained branch details" }),
        )?,
        serde_json::from_value(
            json!({ "type":"compaction", "id":"compaction", "timestamp":"t", "tokensBefore":1234, "firstKeptEntryId":"skill", "summary":"Retained compaction details" }),
        )?,
        persisted(
            "bash",
            BashExecutionMessage {
                command: "false".into(),
                output: "first\r\nlast".into(),
                exit_code: Some(1),
                cancelled: false,
                truncated: false,
                full_output_path: None,
                full_output_error: None,
                timestamp: 0,
                exclude_from_context: Some(true),
            }
            .to_agent_message()?,
        )?,
    ];
    transcript.restore(&entries);
    let frame = plain(&mut transcript, 68);
    assert!(frame.contains("[skill] lint") && frame.contains("Please lint"));
    assert!(frame.contains("[extension]") && frame.contains("Visible card"));
    assert!(frame.contains("Compacted from 1,234 tokens"));
    assert!(frame.contains("$ false") && frame.contains("(exit 1)"));
    assert!(
        !frame.contains("old session")
            && !frame.contains("Invisible card")
            && !frame.contains("Hidden skill body")
            && !frame.contains("Retained branch details")
    );
    transcript.event(&AgentSessionEvent::EntryAppended {
        entry: entries[1].clone(),
    });
    assert!(transcript.toggle_tools());
    let frame = plain(&mut transcript, 68);
    assert_eq!(frame.matches("Visible card").count(), 1);
    assert!(
        frame.contains("Hidden skill body")
            && frame.contains("Retained branch details")
            && frame.contains("Retained compaction details")
    );
    transcript.clear();
    assert!(transcript.render(68).is_empty());
    Ok(())
}

#[test]
fn bash_streaming_keeps_visual_tail_and_persisted_status() -> Result<(), Box<dyn Error>> {
    let mut transcript = Transcript::new(dark()?);
    let id = transcript.begin_bash("printf".into(), false);
    transcript.bash_chunk(id, "first\r");
    transcript.bash_chunk(id, "\nsecond\x1b[31m\x1b[0m");
    let frame = plain(&mut transcript, 30);
    assert!(
        frame.contains("first\n second") && frame.contains("Running..."),
        "{frame}"
    );
    let output = format!("begin\n{}\nend", "abc def ghi ".repeat(80));
    transcript.end_bash(
        id,
        Ok(&BashOutput {
            output,
            cancelled: true,
            truncated: true,
            full_output_path: Some("/tmp/full.log".into()),
            ..BashOutput::default()
        }),
    );
    let frame = plain(&mut transcript, 30);
    assert!(!frame.contains("begin") && frame.contains("end") && frame.contains("(cancelled)"));
    assert!(frame.contains("/tmp/full.log"));
    assert!(
        transcript
            .render(30)
            .iter()
            .all(|line| visible_width(line) <= 30)
    );
    assert!(transcript.toggle_tools());
    assert!(plain(&mut transcript, 30).contains("begin"));
    assert!(
        transcript
            .render(1)
            .iter()
            .all(|line| visible_width(line) <= 1)
    );
    Ok(())
}

#[test]
fn rebuilt_bash_output_follows_ctrl_o_expansion() -> Result<(), Box<dyn Error>> {
    let mut transcript = Transcript::new(Theme::load(std::path::Path::new("."), Some("dark"))?);
    // Rebuilt ! output must follow ctrl+o (default_expanded) state.
    let forty = (1..=40)
        .map(|i| format!("line-{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let entries = vec![persisted(
        "bash",
        BashExecutionMessage {
            command: "seq 1 40".into(),
            output: forty,
            exit_code: Some(0),
            cancelled: false,
            truncated: false,
            full_output_path: None,
            full_output_error: None,
            timestamp: 0,
            exclude_from_context: None,
        }
        .to_agent_message()?,
    )?];
    assert!(transcript.toggle_tools());
    transcript.restore(&entries);
    let expanded_frame = plain(&mut transcript, 80);
    assert!(expanded_frame.contains("line-1\n"));
    assert!(expanded_frame.contains("line-40"));
    assert!(!expanded_frame.contains("to expand"));

    assert!(!transcript.toggle_tools());
    transcript.restore(&entries);
    let collapsed_frame = plain(&mut transcript, 80);
    assert!(!collapsed_frame.contains("line-1\n"));
    assert!(collapsed_frame.contains("line-40"));
    assert!(collapsed_frame.contains("to expand"));
    Ok(())
}

#[test]
fn image_preference_keeps_text_fallback() -> Result<(), Box<dyn Error>> {
    let mut transcript = Transcript::new(dark()?);
    transcript.set_show_images(false);
    let content =
        serde_json::from_value(json!([{"type":"image", "data":"", "mimeType":"image/png"}]))?;
    core(
        &mut transcript,
        AgentEvent::ToolExecutionEnd {
            tool_call_id: "image-1".into(),
            tool_name: "read".into(),
            result: Arc::new(AgentToolResult {
                content,
                ..AgentToolResult::default()
            }),
            is_error: false,
            duration_ms: None,
        },
    );
    assert!(plain(&mut transcript, 70).contains("[Image: [image/png]"));
    Ok(())
}

#[test]
fn markdown_fits_diagrams_and_falls_back_on_resize() -> Result<(), Box<dyn Error>> {
    let mut transcript = Transcript::new(dark()?);
    let text = "Before\n\n```mermaid\nflowchart LR\n A[Start] --> B[Done]\n```\n\nAfter";
    core(
        &mut transcript,
        AgentEvent::MessageStart {
            message: AgentMessage::user(text),
        },
    );
    let wide = plain(&mut transcript, 80);
    assert!(wide.contains("│ Start ├───▶│ Done │"), "{wide}");
    assert!(wide.contains("Before") && wide.contains("After") && !wide.contains("flowchart LR"));
    let narrow = plain(&mut transcript, 17);
    assert!(
        narrow.contains("flowchart LR") && !narrow.contains('▶'),
        "{narrow}"
    );
    assert_eq!(plain(&mut transcript, 80), wide);
    transcript.configure(&json!({ "markdown": { "mermaid": "off" }}));
    assert!(plain(&mut transcript, 80).contains("flowchart LR"));
    Ok(())
}

#[test]
fn normal_empty_reply_clears_copy_but_empty_tool_call_does_not() -> Result<(), Box<dyn Error>> {
    let mut transcript = Transcript::new(dark()?);
    core(
        &mut transcript,
        AgentEvent::MessageEnd {
            message: faux_assistant_message(vec![faux_text("  earlier reply ")]).into(),
        },
    );
    let mut empty = faux_assistant_message(vec![]);
    empty.stop_reason = StopReason::ToolUse;
    core(
        &mut transcript,
        AgentEvent::MessageEnd {
            message: empty.clone().into(),
        },
    );
    assert_eq!(transcript.last_assistant_text(), Some("earlier reply"));
    empty.stop_reason = StopReason::Stop;
    core(
        &mut transcript,
        AgentEvent::MessageEnd {
            message: empty.into(),
        },
    );
    assert_eq!(transcript.last_assistant_text(), None);
    Ok(())
}

#[test]
fn settled_markdown_reuses_rows_until_width_or_theme_changes() -> Result<(), Box<dyn Error>> {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let theme = dark()?;
    let mut transcript = Transcript::new(theme.clone());
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    transcript.transformers.push(MarkdownTransformerDefinition {
        message_types: vec!["user".into()],
        transform: Arc::new(move |source, context| {
            observed.fetch_add(1, Ordering::Relaxed);
            assert_eq!(context["messageType"], "user");
            assert_eq!(context["messageId"], "author");
            assert_eq!(context["isStreaming"], false);
            format!("{source} width={}", context["availableWidth"])
        }),
    });
    transcript.restore(&[persisted("author", AgentMessage::user("input"))?]);
    assert!(plain(&mut transcript, 70).contains("input width=68"));
    assert!(plain(&mut transcript, 70).contains("input width=68"));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert!(plain(&mut transcript, 50).contains("input width=48"));
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    theme.select("light")?;
    assert!(plain(&mut transcript, 50).contains("input width=48"));
    assert_eq!(calls.load(Ordering::Relaxed), 3);
    Ok(())
}

#[test]
fn final_mermaid_mode_waits_for_assistant_completion() -> Result<(), Box<dyn Error>> {
    let mut transcript = Transcript::new(dark()?);
    transcript.configure(&json!({ "markdown": { "mermaid": "final" }}));
    let assistant =
        faux_assistant_message(vec![faux_text("```mermaid\nflowchart LR\n A --> B\n```")]);
    core(
        &mut transcript,
        AgentEvent::MessageStart {
            message: assistant.clone().into(),
        },
    );
    assert!(plain(&mut transcript, 70).contains("flowchart LR"));
    core(
        &mut transcript,
        AgentEvent::MessageEnd {
            message: assistant.into(),
        },
    );
    let frame = plain(&mut transcript, 70);
    assert!(frame.contains("│ A ├───▶│ B │") && !frame.contains("flowchart LR"));
    Ok(())
}

#[test]
fn persisted_edit_result_is_paired_with_original_call_and_highlights_changes()
-> Result<(), Box<dyn Error>> {
    let mut transcript = Transcript::new(dark()?);
    let call = faux_assistant_message(vec![faux_tool_call(
        "edit",
        Map::from_iter([("path".to_owned(), json!("file.rs"))]),
        Some("edit-1".into()),
    )]);
    let result: ToolResultMessage = serde_json::from_value(json!({
        "role": "toolResult", "toolName": "edit", "toolCallId": "edit-1",
        "content": [{"type":"text","text":"Edited the file"}],
        "details": {"diff":"-10    let old = 3;\n+10    let new = 3;"},
        "isError":false, "timestamp":0
    }))?;
    transcript.restore(&[persisted("call", call)?, persisted("result", result)?]);
    let plain = plain(&mut transcript, 80);
    assert_eq!(plain.matches("edit file.rs").count(), 1);
    assert!(plain.contains("-10    let old = 3;") && plain.contains("+10    let new = 3;"));
    assert!(!plain.contains("Edited the file"));
    let styled = transcript.render(80).join("\n");
    assert!(styled.contains("\x1b[7mold\x1b[27m") && styled.contains("\x1b[7mnew\x1b[27m"));
    Ok(())
}

struct CapabilityGuard(std::sync::MutexGuard<'static, ()>);

impl CapabilityGuard {
    fn lock() -> Self {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        Self(LOCK.lock().unwrap_or_else(|error| error.into_inner()))
    }
}

impl Drop for CapabilityGuard {
    fn drop(&mut self) {
        let _ = &self.0;
        pi_rs_tui::set_capability_overrides(
            pi_rs_tui::terminal_image::CapabilityOverrides::default(),
        );
        pi_rs_tui::set_cell_dimensions(pi_rs_tui::CellDimensions::default());
    }
}

fn png_base64(width: u32, height: u32) -> Result<String, Box<dyn Error>> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use image::{ImageEncoder, codecs::png::PngEncoder};

    let mut bytes = Vec::new();
    let pixels = vec![255u8; (width * height * 4) as usize];
    PngEncoder::new(&mut bytes).write_image(
        &pixels,
        width,
        height,
        image::ExtendedColorType::Rgba8,
    )?;
    Ok(STANDARD.encode(bytes))
}

#[test]
fn terminal_image_protocols_emit_kitty_and_iterm2_with_bounds_and_fallback()
-> Result<(), Box<dyn Error>> {
    let _guard = CapabilityGuard::lock();
    let png = png_base64(180, 90)?;
    pi_rs_tui::set_cell_dimensions(pi_rs_tui::CellDimensions {
        width_px: 9,
        height_px: 18,
    });

    let mut transcript = Transcript::new(dark()?);
    transcript.configure(&json!({
        "terminal": {
            "showImages": true,
            "imageWidthCells": 12,
            "imageHeightCells": 8
        }
    }));

    let user_msg = AgentMessage::User(
        pi_rs_ai::UserMessage {
            role: pi_rs_ai::UserRole::User,
            content: StringOrBlocks::Blocks(serde_json::from_value(json!([
                {"type": "image", "data": png, "mimeType": "image/png"}
            ]))?),
            timestamp: 0,
        }
        .into(),
    );
    core(
        &mut transcript,
        AgentEvent::MessageStart {
            message: user_msg.clone(),
        },
    );
    core(
        &mut transcript,
        AgentEvent::MessageEnd { message: user_msg },
    );

    let tool_content = serde_json::from_value(json!([
        {"type": "image", "data": png, "mimeType": "image/png"}
    ]))?;
    core(
        &mut transcript,
        AgentEvent::ToolExecutionEnd {
            tool_call_id: "img-tool".into(),
            tool_name: "read".into(),
            result: Arc::new(AgentToolResult {
                content: tool_content,
                ..AgentToolResult::default()
            }),
            is_error: false,
            duration_ms: None,
        },
    );

    crate::cli::startup_ui::setup_terminal(&json!({
        "terminal": { "images": "Kitty" }
    }));
    let kitty_lines = transcript.render(80);
    let kitty_joined = kitty_lines.join("\n");
    assert!(
        kitty_joined.contains("\x1b_G"),
        "expected Kitty APC sequence"
    );
    assert!(
        kitty_joined.contains("c=12"),
        "expected bounded cell width c=12 in Kitty sequence: {kitty_joined}"
    );
    let kitty_escapes = kitty_joined.matches("\x1b_G").count();
    assert_eq!(
        kitty_escapes, 2,
        "expected both user and tool images to render"
    );

    crate::cli::startup_ui::setup_terminal(&json!({
        "terminal": { "images": "iterm2" }
    }));
    transcript.configure(&json!({
        "terminal": {
            "showImages": true,
            "imageWidthCells": 10
        }
    }));
    let iterm_joined = transcript.render(80).join("\n");
    assert!(
        iterm_joined.contains("\x1b]1337;File="),
        "expected iTerm2 OSC 1337 sequence"
    );
    assert!(iterm_joined.contains("inline=1"));
    assert!(iterm_joined.contains("width=10"));
    assert!(iterm_joined.contains('\x07'));

    crate::cli::startup_ui::setup_terminal(&json!({
        "terminal": { "images": "none" }
    }));
    let disabled_joined = transcript.render(80).join("\n");
    assert!(!disabled_joined.contains("\x1b_G"));
    assert!(!disabled_joined.contains("\x1b]1337;File="));
    assert_eq!(
        disabled_joined
            .matches("[Image: [image/png] 180x90]")
            .count(),
        2
    );
    Ok(())
}
