use pi_rs_p2_tui::{
    components::{Keep, truncate_to_visual_lines},
    messages::{Transcript, transcript_component},
    theme::{BUILTIN_THEMES, TerminalState, create_theme, validate_theme_json},
};
use pi_rs_tui::{Component, TerminalColorMode, strip_terminal_sequences};
use serde_json::json;

#[test]
fn transcript_renders_assistant_diagram_and_keeps_thinking_hidden() -> Result<(), serde_json::Error>
{
    let transcript: Transcript = serde_json::from_value(
        json!({"items":[{"type":"assistant","key":"a","streaming":false,"message":{"content":[{"type":"thinking","thinking":"hidden reasoning"},{"type":"text","text":"```mermaid\nflowchart LR; A[Start] --> B[Done]\n```"}]}}]}),
    )?;
    let view = transcript_component(&transcript, false, false, Default::default());
    let text = strip_terminal_sequences(&view.render(80).join("\n"));
    assert!(
        text.contains("Start") && text.contains("Done") && text.contains('▶'),
        "{text}"
    );
    assert!(!text.contains("hidden reasoning"));
    Ok(())
}

#[test]
fn both_builtin_palettes_style_accents_and_invalid_custom_color_names_the_token()
-> Result<(), Box<dyn std::error::Error>> {
    for (name, json) in BUILTIN_THEMES.iter() {
        let theme = create_theme(json, TerminalColorMode::Truecolor, TerminalState::default())?;
        assert_ne!(theme.fg("accent", "hello"), "hello", "{name}");
        let mut custom = serde_json::to_value(json)?;
        custom["colors"]["accent"] = serde_json::json!(256);
        assert!(
            validate_theme_json("custom", &custom).is_err_and(|err| err.message.contains("accent"))
        );
    }
    Ok(())
}
#[test]
fn collapsed_output_counts_wrapped_rows_and_keeps_the_tail() {
    let result = truncate_to_visual_lines("one\ntwo\nthree", 2, 20, 0, Keep::End);
    assert_eq!(result.skipped_count, 1);
    assert_eq!(
        result
            .visual_lines
            .iter()
            .map(|s| s.trim_end())
            .collect::<Vec<_>>(),
        ["two", "three"]
    );
}

#[test]
fn legacy_draft_uses_main_conversation_and_follow_up_round_trips() -> Result<(), serde_json::Error>
{
    use pi_rs_p2_tui::snapshot::{TuiRuntime, restore_runtime, snapshot_runtime};
    let mut runtime = TuiRuntime::default();
    restore_runtime(
        &mut runtime,
        &json!({"session":"s","screen":"models","showThinking":true,"drafts":[["s","draft"]],"submissions":[["s",{"mode":"followUp","id":"p","text":"later"}]]}),
    );
    let serialized = serde_json::to_value(snapshot_runtime(&runtime))?;
    assert_eq!(serialized["screen"], "chat");
    assert_eq!(serialized["showThinking"], true);
    assert_eq!(serialized["drafts"], json!([["s#1", "draft"]]));
    assert_eq!(serialized["submissions"][0][1]["mode"], "followUp");
    Ok(())
}

#[test]
fn forced_slash_argument_uses_command_completion_and_preserves_the_command()
-> Result<(), Box<dyn std::error::Error>> {
    use pi_rs_p2_tui::completion::FileCompletionProvider;
    use pi_rs_tui::autocomplete::*;
    use std::sync::Arc;
    let slash = CombinedAutocompleteProvider::new(
        vec![SlashCommand {
            name: "model".into(),
            description: None,
            argument_hint: None,
            get_argument_completions: Some(Box::new(|_| {
                Some(vec![AutocompleteItem {
                    value: "local".into(),
                    label: "local".into(),
                    description: None,
                }])
            })),
        }],
        Default::default(),
        None,
    );
    let provider =
        FileCompletionProvider::new(Arc::new(slash), Default::default(), |_| {}, || None);
    let lines = vec!["/model lo".into()];
    let choices = provider
        .get_suggestions(
            &lines,
            0,
            9,
            AutocompleteOptions {
                force: true,
                signal: None,
            },
        )
        .ok_or("missing command completion")?;
    let result = provider.apply_completion(
        lines,
        0,
        9,
        choices.items.first().ok_or("missing model")?,
        &choices.prefix,
    );
    assert_eq!(result.lines[0].trim_end(), "/model local");
    Ok(())
}

#[test]
fn prepending_history_restores_the_same_line_in_the_readers_message()
-> Result<(), Box<dyn std::error::Error>> {
    let old: Transcript = serde_json::from_value(
        json!({"items":[{"type":"user","key":"42-user","entryId":42,"text":"first\nsecond"}]}),
    )?;
    let mut view = transcript_component(&old, false, false, Default::default());
    let anchor = view.anchor_at(80, 1).ok_or("missing anchor")?;
    assert_eq!((anchor.entry_id, anchor.offset), (Some(42), 1));
    view.sync(&serde_json::from_value(json!({"items":[{"type":"failure","key":"3","text":"earlier"},{"type":"user","key":"42-user","entryId":42,"text":"first\nsecond"}]}))?);
    assert_eq!(view.restore_anchor(80, &anchor), Some(2));
    Ok(())
}
