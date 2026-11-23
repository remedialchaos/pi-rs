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
