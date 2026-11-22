use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use pi_rs_ai::{Model, ThinkingLevel};
use serde_json::json;

use super::*;
use crate::core::project_trust::{ProjectTrustStoreEntry, get_project_trust_options};
use crate::core::session_manager::{SessionEntry, SessionInfo};
use crate::core::settings_manager::{CreateOptions, SettingsManager};

fn theme() -> Result<Theme, String> {
    let tmp = tempfile::tempdir().map_err(|error| error.to_string())?;
    Theme::load(tmp.path(), Some("dark")).map_err(|error| error.to_string())
}

#[test]
fn picker_search_reports_original_index_and_settles_once() -> Result<(), String> {
    let result = Rc::new(RefCell::new(Vec::new()));
    let selected = Rc::clone(&result);
    let mut picker = Picker::new(
        "Pick".into(),
        vec![
            "other".into(),
            "alpha".into(),
            "alphabet".into(),
            "alpha".into(),
        ],
        theme()?,
        move |index| selected.borrow_mut().push(index),
    );
    picker.set_focused(true);
    assert!(picker.is_focused());
    picker.handle_input("alpha");
    assert_eq!(picker.visible_order(), &[1, 3, 2]);
    picker.handle_input("\x1b[B");
    picker.handle_input("\r");
    picker.handle_input("\x1b");
    assert_eq!(*result.borrow(), vec![Some(3)]);
    Ok(())
}

#[test]
fn picker_no_match_cannot_select_and_escape_cancels() -> Result<(), String> {
    let result = Rc::new(RefCell::new(Vec::new()));
    let selected = Rc::clone(&result);
    let mut picker = Picker::new(
        "Pick".into(),
        vec!["alpha".into()],
        theme()?,
        move |index| selected.borrow_mut().push(index),
    );
    picker.handle_input("zz");
    picker.handle_input("\r");
    assert!(result.borrow().is_empty());
    picker.handle_input("\x1b");
    assert_eq!(*result.borrow(), vec![None]);
    Ok(())
}

fn session(
    id: &str,
    name: Option<&str>,
    messages: &str,
    modified_millis: i64,
    parent: Option<&str>,
) -> SessionInfo {
    SessionInfo {
        path: PathBuf::from(format!("/sessions/{id}.jsonl")),
        id: id.into(),
        cwd: "/project".into(),
        name: name.map(str::to_owned),
        parent_session_path: parent.map(|id| PathBuf::from(format!("/sessions/{id}.jsonl"))),
        created: String::new(),
        modified: String::new(),
        created_millis: 0,
        modified_millis,
        message_count: 1,
        first_message: messages.into(),
        all_messages_text: messages.into(),
    }
}

#[test]
fn sessions_use_literal_phrases_regex_and_name_priority() -> Result<(), String> {
    let sessions = vec![
        session("new", None, "Look at the NODE\nCVE report", 200, None),
        session(
            "named",
            Some("Node   CVE triage"),
            "Initial report",
            100,
            None,
        ),
        session("unrelated", Some("Notes"), "N O D E", 300, None),
    ];
    use SessionNameFilter::{All, Named};
    use SessionSort::{Recent, Relevance};
    assert_eq!(
        filter_and_sort_sessions(&sessions, "node", Relevance, All)?,
        vec![1, 0]
    );
    assert_eq!(
        filter_and_sort_sessions(&sessions, "node", Recent, All)?,
        vec![0, 1]
    );
    assert_eq!(
        filter_and_sort_sessions(&sessions, "\"node cve\"", Relevance, All)?,
        vec![1, 0]
    );
    assert_eq!(
        filter_and_sort_sessions(&sessions, "re:node\\s+cve", Relevance, Named)?,
        vec![1]
    );
    assert_eq!(
        filter_and_sort_sessions(&sessions, "report triage", Relevance, All)?,
        vec![1]
    );
    assert!(filter_and_sort_sessions(&sessions, "re:[", Recent, All).is_err());
    Ok(())
}

#[test]
fn session_threads_sort_by_descendant_activity_and_keep_cycle_rows() -> Result<(), String> {
    let sessions = vec![
        session("fork", None, "child", 300, Some("root")),
        session("independent", None, "other", 200, None),
        session("root", Some("parent"), "root", 100, None),
        session("cycle-a", None, "a", 10, Some("cycle-b")),
        session("cycle-b", None, "b", 9, Some("cycle-a")),
    ];
    assert_eq!(
        filter_and_sort_sessions(&sessions, "", SessionSort::Threaded, SessionNameFilter::All)?,
        vec![2, 0, 1, 3, 4]
    );
    assert_eq!(
        filter_and_sort_sessions(
            &sessions,
            "",
            SessionSort::Threaded,
            SessionNameFilter::Named
        )?,
        vec![2]
    );
    Ok(())
}

fn message(id: &str, parent: Option<&str>, role: &str, content: &str) -> SessionEntry {
    SessionEntry::Message {
        id: id.into(),
        parent_id: parent.map(str::to_owned),
        timestamp: String::new(),
        message: json!({"role": role, "content":[{"type":"text","text":content}]}),
    }
}

#[test]
fn tree_prefers_active_path_and_preselects_nearest_visible_ancestor() -> Result<(), String> {
    let entries = vec![
        message("root", None, "user", "Begin here"),
        message("left", Some("root"), "assistant", "Inactive fork"),
        message("right", Some("root"), "user", "Current question"),
        SessionEntry::ModelChange {
            id: "leaf".into(),
            parent_id: Some("right".into()),
            timestamp: String::new(),
            provider: "provider".into(),
            model_id: "model".into(),
        },
    ];
    let rows = tree_items(&entries, Some("leaf"));
    assert_eq!(
        rows.iter()
            .map(|row| row.value.as_str())
            .collect::<Vec<_>>(),
        ["root", "right", "left"]
    );
    assert!(rows[1].label.contains("• user: Current question"));
    let forks = user_message_items(&entries);
    assert_eq!(
        forks
            .iter()
            .map(|row| row.value.as_str())
            .collect::<Vec<_>>(),
        ["root", "right"]
    );
    let output = Rc::new(RefCell::new(Vec::new()));
    let result = Rc::clone(&output);
    let mut picker = TreePicker::new(
        &entries,
        Some("leaf"),
        TreeFilter::Default,
        theme()?,
        move |event| result.borrow_mut().push(event),
    );
    picker.handle_input("\r");
    assert_eq!(*output.borrow(), [TreeAction::Select("right".into())]);
    Ok(())
}

#[test]
fn trust_preselects_saved_parent_and_session_only_never_persists() {
    let cwd = Path::new("/project/subdir");
    let expected = get_project_trust_options(cwd, true);
    let saved = expected
        .get(1)
        .and_then(|option| option.saved_path.clone())
        .map(|path| ProjectTrustStoreEntry {
            path,
            decision: true,
        });
    let (rows, index) = trust_items(cwd, saved.as_ref(), true);
    assert_eq!(index, 1);
    assert!(rows[index].label.starts_with("✓"));
    let updates = &rows[index].value.updates;
    assert_eq!(updates.len(), 2);
    assert_eq!(updates[1].decision, None);
    assert_eq!(
        rows.iter()
            .filter(|row| row.value.updates.is_empty())
            .count(),
        2
    );
    let (permanent, _) = trust_items(cwd, saved.as_ref(), false);
    assert!(permanent.iter().all(|row| !row.value.updates.is_empty()));
}

#[test]
fn settings_emit_typed_updates_and_hide_unsupported_images() -> Result<(), String> {
    let settings = SettingsManager::in_memory(json!({}), CreateOptions::default());
    assert!(
        settings_items(&settings, false)
            .iter()
            .all(|row| row.value != ["terminal", "showImages"])
    );
    assert!(
        settings_items(&settings, true)
            .iter()
            .any(|row| row.value == ["terminal", "showImages"])
    );
    let result = Rc::new(RefCell::new(Vec::new()));
    let output = Rc::clone(&result);
    let mut picker = SettingsPicker::new(&settings, theme()?, move |change| {
        output.borrow_mut().push(change)
    });
    picker.handle_input("\r");
    assert_eq!(
        *result.borrow(),
        vec![Some(SettingChange {
            path: vec!["compaction".into(), "enabled".into()],
            value: json!(false)
        })]
    );
    picker.handle_input("\x1b");
    assert_eq!(result.borrow().last(), Some(&None));
    Ok(())
}

fn model(provider: &str, id: &str, name: &str) -> Result<Model, String> {
    serde_json::from_value(json!({"provider":provider,"id":id,"name":name,"api":"openai-responses","baseUrl":"https://example.com","reasoning":false,"input":["text"],"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":4096,"maxTokens":1024})).map_err(|error| error.to_string())
}

#[test]
fn model_rows_preserve_index_and_scoped_order_is_explicit() -> Result<(), String> {
    let models = vec![
        model("b", "mini", "Mini")?,
        model("a", "large", "Large Preview")?,
        model("a", "mini", "Mini")?,
    ];
    let rows = model_items(&models, Some(&models[0]), Some(("a", "mini")));
    assert_eq!(
        rows.iter().map(|row| row.value).collect::<Vec<_>>(),
        [0, 2, 1]
    );
    assert!(rows[2].label.contains("Large Preview"));
    assert!(rows[1].label.ends_with("default"));
    let mut scope = ScopedModels::new(&models, None);
    scope.toggle("a/large");
    assert_eq!(
        scope.enabled().map(|value| value.join(",")),
        Some("b/mini,a/mini".into())
    );
    assert!(scope.move_by("a/mini", -1));
    assert_eq!(
        scope.enabled().map(|value| value.join(",")),
        Some("a/mini,b/mini".into())
    );
    scope.toggle("a/large");
    assert!(scope.enabled().is_none());
    assert_eq!(
        thinking_items(
            &[ThinkingLevel::Off, ThinkingLevel::High],
            ThinkingLevel::High,
            Some(ThinkingLevel::Off)
        )[1]
        .value,
        ThinkingLevel::High
    );
    Ok(())
}
