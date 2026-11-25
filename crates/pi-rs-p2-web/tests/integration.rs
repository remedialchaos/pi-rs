//! Cross-module integration tests for Milestone 3.2-web (offline, native).

use pi_rs_p2_web::activity::detail::describe;
use pi_rs_p2_web::activity::model::{Step, StepStatus, ToolActivity, ToolActivityTable, summary};
use pi_rs_p2_web::assets;
use pi_rs_p2_web::chord_bridge::{
    FrameHeader, decode_header, decode_part, decode_varint, encode_header, encode_part,
    encode_varint,
};
use pi_rs_p2_web::command_menu::highlight::{Highlight, ListOption};
use pi_rs_p2_web::command_menu::r#match::match_score;
use pi_rs_p2_web::command_menu::recents::Recents;
use pi_rs_p2_web::cost::{AssistantCost, axis_cost, precise_cost};
use pi_rs_p2_web::gestures::sheet_drag::{rubber_band, scrim_opacity};
use pi_rs_p2_web::markdown::{render_html, safe_href};
use pi_rs_p2_web::state::chat_context::relative_path;
use pi_rs_p2_web::state::conversation_cache::{TranscriptItem, TranscriptView, kept_view};
use pi_rs_p2_web::state::file_completion::{Popover, extract_at_prefix, parse_path_prefix};

#[test]
fn css_assets_are_embedded_as_drop_in_stylesheets() {
    assert_eq!(assets::STYLESHEETS.len(), 7);
    for (name, css) in assets::STYLESHEETS {
        assert!(!css.is_empty(), "{name}");
    }
    // The server serves these exact bytes; byte equality with the embedded copy is the contract.
    assert_eq!(assets::ELANOR_CSS, assets::STYLESHEETS[0].1);
    assert!(assets::STYLE_CSS.contains("--bg"));
}

#[test]
fn command_menu_flow_from_query_to_highlight_to_recents() {
    let commands = ["New session", "Open terminal", "Change theme"];
    let ranked =
        pi_rs_p2_web::command_menu::r#match::rank_by(&commands, |text| match_score("term", text));
    assert_eq!(ranked, vec![&"Open terminal"]);

    let mut highlight = Highlight::new(vec![
        ListOption {
            id: "new".into(),
            preferred: false,
        },
        ListOption {
            id: "terminal".into(),
            preferred: false,
        },
    ]);
    highlight.move_highlight(1);
    assert_eq!(highlight.highlighted_id(), Some("terminal"));
    assert_eq!(highlight.enter("term", 100), Some(1));

    let mut recents = Recents::default();
    recents.record("terminal");
    assert_eq!(recents.ids(), &["terminal"]);
    assert_eq!(Recents::read(Some(&recents.to_json())), recents);
}

#[test]
fn activity_projection_from_steps_to_summary() {
    fn edit(count: usize) -> String {
        if count == 1 {
            "edited a file".into()
        } else {
            format!("edited {count} files")
        }
    }
    let mut tools = ToolActivityTable::new();
    tools.insert(
        "edit".into(),
        ToolActivity {
            summary: Some(edit),
            ..Default::default()
        },
    );
    let steps = vec![
        Step::new("1", "edit", StepStatus::Done),
        Step::new("2", "edit", StepStatus::Error),
    ];
    assert_eq!(summary(&steps, &tools, true), "Edited 2 files · 1 failed");
    let described = describe(&steps[0], &tools);
    assert_eq!(described.label.verb.as_deref(), Some("edit"));
}

#[test]
fn gesture_geometry_stays_within_bounds() {
    let banded = rubber_band(1000.0, 100.0, 0.8);
    assert!(banded > 0.0 && banded < 100.0);
    assert_eq!(rubber_band(10.0, 0.0, 0.8), 0.0);
    assert!((scrim_opacity(50.0, 100.0) - 0.5).abs() < f64::EPSILON);
}

#[test]
fn cost_formatting_and_markdown_safety() {
    let costs = [AssistantCost {
        entry_id: "1".into(),
        cost: 0.05,
        cache_hit: Some(0.25),
        model: "test".into(),
        stop_reason: "stop".into(),
    }];
    assert_eq!(precise_cost(costs[0].cost), "$0.0500");
    assert_eq!(axis_cost(0.0), "$0");
    let html = render_html("**safe** <img src=x onerror=alert(1)> [go](javascript:x)");
    assert!(html.contains("<strong>safe</strong>"));
    // Raw HTML is escaped to text: the word "onerror" may appear as text, but no tag does.
    assert!(!html.contains("<img"));
    assert!(!html.contains("<script"));
    assert!(!html.contains("href=\"javascript:"));
    assert_eq!(safe_href("mailto:a@b.c").as_deref(), Some("mailto:a@b.c"));
}

#[test]
fn binary_frame_and_varint_round_trip_through_all_layers() {
    let mut bytes = vec![];
    encode_varint(300, &mut bytes);
    assert_eq!(decode_varint(&bytes).unwrap().0, 300);
    let header = FrameHeader {
        kind: 5,
        origin: Some(300),
        trace: Some("trace".into()),
        old: true,
    };
    let frame = encode_header(&header);
    assert_eq!(decode_header(&frame).unwrap(), header);
    let part = encode_part(b"payload", false);
    assert_eq!(decode_part(&part).unwrap().0, b"payload".to_vec());
}

#[test]
fn conversation_window_and_path_projection() {
    let view = TranscriptView {
        conversation: "c".into(),
        items: vec![TranscriptItem::Entry(
            pi_rs_p2_web::state::conversation_cache::Entry { id: 1, size: 10 },
        )],
        docs: Default::default(),
    };
    let kept = kept_view(&view);
    assert_eq!(kept.entries.len(), 1);
    assert_eq!(
        relative_path(Some("/home/pi/p"), "/home/pi/p/src/a.rs"),
        "src/a.rs"
    );
}

#[test]
fn completion_popover_and_key_handling() {
    let mut popover = Popover {
        items: pi_rs_p2_web::state::file_completion::at_items(
            &[("src/main.rs".into(), false)],
            false,
        ),
        prefix: "@".into(),
        selected: 0,
        hint: None,
    };
    assert_eq!(extract_at_prefix("see @src/ma"), Some("@src/ma".into()));
    assert_eq!(parse_path_prefix("@src/ma"), ("src/ma".into(), true, false));
    assert_eq!(popover.items.len(), 1);
    assert!(popover.on_key(
        pi_rs_p2_web::state::file_completion::PopoverKey::Enter,
        false
    ));
}

#[test]
fn all_builtin_contracts_encode_canonical_hold_frames() {
    use pi_rs_p2_web::browser_plugins::{ALL_CONTRACTS, hold_for_contract};
    use pi_rs_p2_web::chord_bridge::decode_varint;

    assert_eq!(ALL_CONTRACTS.len(), 16);
    for (idx, spec) in ALL_CONTRACTS.iter().enumerate() {
        let hold_id = (idx + 1) as u64;
        let frame = hold_for_contract(hold_id, spec, Some("s-1"), Some(2), Some("web"));
        assert_eq!(frame[0], 1, "Hold frame kind for {}", spec.name);
        let (decoded_id, n1) = decode_varint(&frame[1..]).unwrap();
        assert_eq!(decoded_id, hold_id);
        let mut pos = 1 + n1;
        let (name_len, n2) = decode_varint(&frame[pos..]).unwrap();
        pos += n2;
        assert_eq!(&frame[pos..pos + name_len as usize], spec.name.as_bytes());
        pos += name_len as usize;
        let (version, n3) = decode_varint(&frame[pos..]).unwrap();
        assert_eq!(version, spec.version, "version for {}", spec.name);
        pos += n3;
        let fp = u64::from_be_bytes(frame[pos..pos + 8].try_into().unwrap());
        assert_eq!(fp, spec.fingerprint, "fingerprint for {}", spec.name);
    }
}

#[test]
fn rich_chat_projection_and_plugin_decoders_and_fault_isolation() {
    use pi_rs_p2_web::activity::model::{StepStatus, ToolActivityTable};
    use pi_rs_p2_web::browser_plugins::{
        ComposerAttachments, HoldStatus, HoldTracker, P2_CHAT, P2_MODELS, P2_SESSIONS,
        ProjectedRole, SidePanel, builtin_commands, decode_artifacts_view, decode_costs_view,
        decode_mcp_view, decode_models_view, decode_session_status_view, decode_settings_view,
        decode_sharing_view, decode_terminals_list, decode_tree_view, decode_voice_view,
        project_chat_transcript,
    };
    use pi_rs_p2_web::ui::terminal::VoiceState;
    use serde_json::json;

    let view = json!({
        "entries": [
            { "id": 1, "data": { "author": { "kind": "owner" }, "attachments": [], "text": "hello" }, "model": [] },
            { "id": 2, "data": {}, "model": [{
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "checking" },
                    { "type": "toolCall", "id": "call-1", "name": "read", "arguments": { "path": "Cargo.toml" } }
                ]
            }] },
            { "id": 3, "data": {}, "model": [{
                "role": "toolResult",
                "toolCallId": "call-1",
                "toolName": "read",
                "isError": true,
                "durationMs": 15,
                "content": [{ "type": "text", "text": "not found" }]
            }] }
        ],
        "docs": {
            "pi.live": {
                "generation": {
                    "message": { "role": "assistant", "content": "working..." }
                }
            }
        }
    });
    let projected = project_chat_transcript(&view, &ToolActivityTable::new());
    assert_eq!(projected.len(), 3);
    assert_eq!(projected[0].role, ProjectedRole::User);
    assert_eq!(projected[1].activities.len(), 1);
    assert_eq!(projected[1].activities[0].step.status, StepStatus::Error);
    assert!(projected[2].streaming);

    assert_eq!(decode_models_view(Some(&json!([{"provider":"openai","modelId":"gpt-5","name":"GPT-5","contextWindow":128000,"thinkingLevels":["low"]}])), None, None, None).models.len(), 1);
    assert_eq!(
        decode_settings_view(Some(&json!({"detail": 2})), None)
            .entries
            .get("detail")
            .map(String::as_str),
        Some("2")
    );
    assert_eq!(
        decode_mcp_view(
            Some(
                &json!([{"id":"1","server":"s","source":"owner","status":"connected","tools":[]}])
            ),
            None,
            None
        )
        .instances
        .len(),
        1
    );
    assert_eq!(
        decode_sharing_view(
            None,
            Some(&json!([{"mode":"full","token":"t","url":"u"}])),
            None,
            None
        )
        .links
        .len(),
        1
    );
    assert!(
        decode_session_status_view(Some(&json!({"s1":{"running":true,"busy":[1],"tasks":1}})))
            .statuses
            .contains_key("s1")
    );
    assert_eq!(decode_costs_view(&json!([{"entryId":"e1","cost":0.1,"model":"m","provider":"p","stopReason":"stop","timestamp":1}])).len(), 1);
    assert_eq!(
        decode_artifacts_view(Some(
            &json!([{"id":"a1","kind":"md","title":"A","version":1}])
        ))
        .len(),
        1
    );
    assert_eq!(
        decode_tree_view(
            &json!({"current":1,"main":1,"branches":[{"conversation":1,"label":"main"}],"nodes":[]})
        )
        .branches
        .len(),
        1
    );
    assert_eq!(decode_terminals_list(&json!(["term-1"])), vec!["term-1"]);
    assert_eq!(
        decode_voice_view(Some(&json!(false)), None, None, None).state,
        VoiceState::Idle
    );

    let mut attachments = ComposerAttachments::default();
    attachments.add_files(
        &[("notes.txt".into(), 100, false)],
        |i| format!("k{i}"),
        |_| false,
    );
    assert_eq!(
        attachments
            .referenced_for_draft("see [notes.txt](attachment:k0)")
            .len(),
        1
    );
    attachments.remove("k0");
    assert!(attachments.files.is_empty());

    assert!(SidePanel::Cost.is_open());
    assert!(!SidePanel::None.is_open());
    assert!(builtin_commands().iter().any(|c| c.id == "panel-models"));

    let mut tracker = HoldTracker::default();
    tracker.register(2, P2_SESSIONS.name, true);
    tracker.register(3, P2_CHAT.name, true);
    tracker.register(4, P2_MODELS.name, false);
    assert_eq!(tracker.mark_failed(4, "owner only".into()), None);
    assert_eq!(
        tracker.status_of(P2_MODELS.name),
        Some(&HoldStatus::Unavailable("owner only".into()))
    );
    assert_eq!(
        tracker.mark_failed(3, "chat gone".into()),
        Some("chat gone".into())
    );
}
