use indexmap::IndexMap;
use pi_rs_chord::{
    Result,
    context::Context,
    host::*,
    json::JsonValue,
    state::{Batch, NativeSource},
    surfaces::*,
    tui::*,
    web::*,
};
use pi_rs_tui::Terminal;
use std::{
    collections::BTreeSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
#[tokio::test]
async fn principal_choices_follow_commits_and_survive_reload_and_host_restart() -> Result<()> {
    use pi_rs_chord::{sqlite::SqliteDatabase, store::StoreEngine};
    let db = SqliteDatabase::open(":memory:", Default::default()).await?;
    let store = StoreEngine::open(db).await?;
    let options = HostOptions {
        store: Some(store),
        principal: Some(serde_json::json!("server")),
        address: Some(JsonValue::from_serde(
            serde_json::json!({"kind":"server","params":{}}),
        )),
        ..Default::default()
    };
    let contract = Arc::new(locale_choice()?);
    let host = create_host(options.clone())?;
    let ctx = Context::background();
    host.load(
        vec![PluginFacet::new("locale", locale_choice_facet()?)],
        &ctx,
    )
    .await?;
    let caller = |principal| Caller {
        principal: Some(principal),
        incarnation: "client".into(),
    };
    let a = host.hold_as(
        contract.clone(),
        JsonValue::Object(Default::default()),
        caller(serde_json::json!({"b":2,"a":1})),
        &ctx,
    )?;
    let b = host.hold_as(
        contract.clone(),
        JsonValue::Object(Default::default()),
        caller(serde_json::json!("other")),
        &ctx,
    )?;
    tokio::time::timeout(std::time::Duration::from_secs(3), a.wait(&ctx))
        .await
        .unwrap()?;
    b.wait(&ctx).await?;
    let readable = a.member("pick")?;
    assert_eq!(readable.value()?.to_json(), JsonValue::Null);
    a.call("choose", JsonValue::String("ar".into()), &ctx)
        .await?;
    assert_eq!(readable.value()?.to_json(), JsonValue::String("ar".into()));
    assert_eq!(b.read("pick")?, JsonValue::Null);
    host.reload(
        vec![PluginFacet::new("locale", locale_choice_facet()?)],
        &ctx,
    )
    .await?;
    assert_eq!(readable.value()?.to_json(), JsonValue::String("ar".into()));
    a.release();
    b.release();
    host.close(&ctx).await?;
    drop(host);
    let host = create_host(options)?;
    host.load(
        vec![PluginFacet::new("locale", locale_choice_facet()?)],
        &ctx,
    )
    .await?;
    let a = host.hold_as(
        contract,
        JsonValue::Object(Default::default()),
        caller(serde_json::json!({"a":1,"b":2})),
        &ctx,
    )?;
    a.wait(&ctx).await?;
    assert_eq!(a.read("pick")?, JsonValue::String("ar".into()));
    assert!(
        a.call("choose", JsonValue::String("invalid_tag".into()), &ctx)
            .await
            .is_err()
    );
    host.close(&ctx).await?;
    Ok(())
}
#[tokio::test]
async fn loaded_digest_identity_skips_preparation_and_failed_updates_keep_old() -> Result<()> {
    let host = create_host(Default::default())?;
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let facet = define_facet(move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });
    let prepared = facet.clone();
    let (loaded, _) = LoadedFacets::load(
        host.clone(),
        vec![],
        vec![SetEntry {
            name: "plugin".into(),
            digest: Some("one".into()),
            prepare: Arc::new(move || {
                Ok(PreparedFacet {
                    facet: prepared.clone(),
                    digest: "one".into(),
                })
            }),
        }],
        &Context::background(),
    )
    .await?;
    let report = loaded
        .reload(
            vec![SetEntry {
                name: "plugin".into(),
                digest: Some("one".into()),
                prepare: Arc::new(|| {
                    Err(pi_rs_chord::Error::Invalid(
                        "must not prepare unchanged".into(),
                    ))
                }),
            }],
            &Context::background(),
        )
        .await?;
    assert!(report.changed.is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let report = loaded
        .reload(
            vec![SetEntry {
                name: "plugin".into(),
                digest: Some("two".into()),
                prepare: Arc::new(|| Err(pi_rs_chord::Error::Invalid("bad build".into()))),
            }],
            &Context::background(),
        )
        .await?;
    assert_eq!(report.failed.len(), 1);
    assert_eq!(loaded.digest("plugin").as_deref(), Some("one"));
    let state = host.page_state("editor", "draft".to_string())?;
    state.set("edited".into(), &Context::background())?;
    host.reload(vec![], &Context::background()).await?;
    assert_eq!(
        host.page_state("editor", "empty".to_string())?.value(),
        "edited"
    );
    host.close(&Context::background()).await?;
    Ok(())
}
#[test]
fn theme_and_locale_followers_track_choices_modes_and_stop() -> Result<()> {
    let batch = Batch::new(|_| {});
    let ctx = Context::background();
    let mut dark = fallback_theme(ThemeMode::Dark);
    dark.id = "custom".into();
    dark.colors.insert("accent".into(), "#123456".into());
    let themes = NativeSource::new(batch.clone(), vec![dark.clone()], |a, b| a == b);
    let pick = NativeSource::new(batch.clone(), ThemePick::default(), |a, b| a == b);
    let follow = ThemeFollower::follow(themes.clone(), pick, ThemeMode::Dark, batch.clone(), &ctx)?;
    assert_eq!(follow.source.value().id, "custom");
    follow.set_mode(ThemeMode::Light, &ctx)?;
    assert_eq!(follow.source.value().mode, ThemeMode::Light);
    follow.stop();
    follow.set_mode(ThemeMode::Dark, &ctx)?;
    assert_eq!(follow.source.value().mode, ThemeMode::Light);
    assert_eq!(
        terminal_color(&dark, "accent", false, ColorDepth::Truecolor),
        "38;2;18;52;86"
    );
    dark.tui.insert("accent".into(), "007".into());
    assert_eq!(
        terminal_color(&dark, "accent", true, ColorDepth::Indexed),
        "48;5;007"
    );
    let offered = NativeSource::new(batch.clone(), vec!["de".into(), "en".into()], |a, b| a == b);
    let pick = NativeSource::new(batch.clone(), None, |a, b| a == b);
    let follow =
        LanguageFollower::follow(offered, pick.clone(), vec!["de-CH".into()], batch, &ctx)?;
    assert_eq!(follow.locale.value(), "de");
    pick.set(Some("en".into()), &ctx)?;
    assert_eq!(follow.locale.value(), "en");
    assert_eq!(canonical_locale("pt-br").as_deref(), Some("pt-BR"));
    assert_eq!(direction_of("ar"), "rtl");
    assert_eq!(direction_of("ar-Latn"), "ltr");
    assert!(canonical_locale("not_a_tag").is_none());
    Ok(())
}
#[test]
fn contributed_catalog_override_fallback_and_lexical_language_are_scoped() -> Result<()> {
    let own = Arc::new(IndexMap::from([(
        "saved".into(),
        MessageValue::Text("Saved".into()),
    )]));
    let de = Arc::new(IndexMap::from([(
        "saved".into(),
        MessageValue::Text("Gespeichert".into()),
    )]));
    let messages = Messages::define(
        "notes",
        IndexMap::from([("en".into(), own.clone()), ("de".into(), de)]),
    )?;
    let overrides = Arc::new(IndexMap::from([(
        "saved".into(),
        MessageValue::Text("Erledigt".into()),
    )]));
    let translations = compose_translations(
        vec![
            TranslationOwner {
                plugin: "a".into(),
                value: Translation {
                    namespace: "notes".into(),
                    locale: "de".into(),
                    messages: own,
                    override_catalog: false,
                },
            },
            TranslationOwner {
                plugin: "z".into(),
                value: Translation {
                    namespace: "notes".into(),
                    locale: "de".into(),
                    messages: overrides,
                    override_catalog: true,
                },
            },
        ],
        |_| {},
    );
    let reports = Arc::new(Mutex::new(Vec::new()));
    let log = reports.clone();
    let language = create_language("de-CH", Arc::new(translations), move |report| {
        log.lock().unwrap().push(report)
    })?;
    assert_eq!(messages.render("saved", &[], Some(&language))?, "Erledigt");
    assert_eq!(reports.lock().unwrap()[0].kind, "fallback");
    in_language(Some(language), || {
        assert_eq!(current_language().unwrap().locale, "de-CH");
        in_language(None, || assert!(current_language().is_none()));
        assert!(current_language().is_some());
    });
    assert!(current_language().is_none());
    Ok(())
}
#[test]
fn connection_backoff_stability_wake_and_stale_generations_match_protocol() {
    let mut connection = ConnectionLoop::default();
    assert!(
        connection
            .connect(0)
            .contains(&ConnectionEffect::Open { generation: 1 })
    );
    let closed = connection.closed(1, 0, true);
    assert!(closed.contains(&ConnectionEffect::Timer {
        at: 250,
        generation: 1,
        kind: TimerKind::Retry
    }));
    assert!(closed.contains(&ConnectionEffect::AskRetry));
    assert!(
        connection
            .wake(20)
            .contains(&ConnectionEffect::Open { generation: 2 })
    );
    assert!(connection.timer(TimerKind::Retry, 1, 250).is_empty());
    connection.opened(2, 30);
    let effects = connection.wake(26_000);
    assert!(effects.contains(&ConnectionEffect::Timer {
        at: 27_000,
        generation: 2,
        kind: TimerKind::Stale
    }));
    assert_eq!(
        connection.timer(TimerKind::Stale, 2, 27_000),
        vec![ConnectionEffect::Close { generation: 2 }]
    );
    assert!(
        connection
            .closed(2, 27_000, true)
            .contains(&ConnectionEffect::Timer {
                at: 27_000,
                generation: 2,
                kind: TimerKind::Retry
            })
    );
    connection.timer(TimerKind::Retry, 2, 27_000);
    connection.retry_answer(false);
    assert!(connection.wake(30_000).is_empty());
}
#[test]
fn web_styles_adopt_only_committed_plugins_and_remove_old_rules_after_draw() {
    let set = ScriptSet {
        base: "base".into(),
        combined: "all".into(),
        plugins: vec![ScriptEntry {
            name: "a".into(),
            digest: "2".into(),
            url: "a.js".into(),
            css: Some("new.css".into()),
        }],
    };
    let mut styles = PageStyles::present(IndexMap::from([(
        "a".into(),
        ("old.css".into(), SheetState::Loaded),
    )]));
    assert!(matches!(
        styles.prepare(&set)[0],
        PageEffect::PrepareSheet { .. }
    ));
    styles.loaded("a", "new.css", true);
    let effects = styles.settle(&set, &BTreeSet::from(["a".into()]));
    assert_eq!(styles.current("a"), Some("old.css"));
    assert!(matches!(effects[0], PageEffect::RemoveSheet { .. }));
    styles.prepare(&set);
    styles.loaded("a", "new.css", true);
    let effects = styles.settle(&set, &BTreeSet::new());
    assert_eq!(styles.current("a"), Some("new.css"));
    assert!(effects.contains(&PageEffect::AfterDrawn(vec![(
        "a".into(),
        "old.css".into()
    )])));
    let mut scripts = PageScripts::new("base", vec![]);
    scripts.fetch(set.plugins[0].clone());
    assert!(scripts.registered("a", "wrong").is_none());
    assert!(scripts.loaded("a", "2").is_err());
    scripts.fetch(set.plugins[0].clone());
    assert!(scripts.registered("a", "2").is_some());
    assert!(scripts.loaded("a", "2").is_ok());
    assert!(!wakes_connection(WakeEvent::Restored { persisted: false }));
}
#[test]
fn terminal_adapter_keeps_utf16_scalars_and_escape_sequences_whole() -> Result<()> {
    let output = Arc::new(Mutex::new(Vec::new()));
    let sent = output.clone();
    let channel = SandboxChannel::new(
        BridgeStart {
            base: "base".into(),
            combined: "".into(),
            columns: 80,
            rows: 24,
            colors: ColorDepth::Truecolor,
            mode: ThemeMode::Dark,
            locales: vec!["en".into()],
        },
        move |message| {
            sent.lock().unwrap().push(message);
            Ok(())
        },
    );
    let mut terminal = channel.terminal();
    channel.receive(LauncherMessage::Resize {
        columns: 120,
        rows: 40,
    })?;
    assert_eq!((terminal.width(), terminal.height()), (120, 40));
    channel.receive(LauncherMessage::Input {
        data: "é\r".into()
    })?;
    assert_eq!(terminal.read_available(), "é\r".as_bytes());
    let text = format!(
        "{}𝄞\x1b]8;;https://example.invalid\x1b\\link\x1b]8;;\x1b\\",
        "a".repeat(FRAME_UNITS - 1)
    );
    let frames = split_frames(&text);
    assert_eq!(frames.concat(), text);
    assert_eq!(frames[0].len(), FRAME_UNITS - 1);
    terminal.write(&text);
    assert!(terminal.take_error().is_none());
    let writes = output
        .lock()
        .unwrap()
        .iter()
        .filter_map(|m| {
            if let SandboxMessage::Write { data } = m {
                Some(data.clone())
            } else {
                None
            }
        })
        .collect::<String>();
    assert_eq!(writes, text);
    assert_eq!(split_frames(""), vec![""]);
    Ok(())
}
#[tokio::test]
async fn sandbox_requests_and_socket_frames_route_by_id_and_cancel_cleanly() -> Result<()> {
    let sent = Arc::new(Mutex::new(Vec::new()));
    let output = sent.clone();
    let channel = SandboxChannel::new(
        BridgeStart {
            base: "b".into(),
            combined: "".into(),
            columns: 80,
            rows: 24,
            colors: ColorDepth::Indexed,
            mode: ThemeMode::Light,
            locales: vec![],
        },
        move |m| {
            output.lock().unwrap().push(m);
            Ok(())
        },
    );
    let worker = channel.clone();
    let request = tokio::spawn(async move {
        worker
            .request(BridgeRequest::Base, &Context::background())
            .await
    });
    tokio::task::yield_now().await;
    let id = match &sent.lock().unwrap()[0] {
        SandboxMessage::Request { id, .. } => *id,
        _ => 0,
    };
    channel.receive(LauncherMessage::Reply {
        id,
        value: serde_json::json!("base"),
    })?;
    assert_eq!(
        request.await.unwrap()?,
        BridgeReply::Value(serde_json::json!("base"))
    );
    let socket = channel.socket()?;
    socket.send(vec![1, 2])?;
    let id = sent
        .lock()
        .unwrap()
        .iter()
        .find_map(|m| {
            if let SandboxMessage::SocketOpen { socket } = m {
                Some(*socket)
            } else {
                None
            }
        })
        .unwrap();
    assert!(
        !sent
            .lock()
            .unwrap()
            .iter()
            .any(|m| matches!(m, SandboxMessage::SocketSend { .. }))
    );
    channel.receive(LauncherMessage::SocketOpened { socket: id })?;
    assert!(sent.lock().unwrap().contains(&SandboxMessage::SocketSend {
        socket: id,
        bytes: vec![1, 2]
    }));
    let received = Arc::new(Mutex::new(Vec::new()));
    let input = received.clone();
    socket.on_message(Arc::new(move |bytes| input.lock().unwrap().push(bytes)));
    channel.receive(LauncherMessage::SocketMessage {
        socket: id,
        bytes: vec![3],
    })?;
    assert_eq!(*received.lock().unwrap(), vec![vec![3]]);
    channel.close();
    assert!(socket.closed());
    Ok(())
}
#[test]
fn script_contracts_and_registration_preserve_protocol_names_and_escaping() -> Result<()> {
    assert_eq!(web_scripts()?.name(), "chord.web.scripts");
    assert_eq!(tui_scripts()?.name(), "chord.tui.scripts");
    assert_eq!(
        registered_source("a\"", "digest", "function(){}")?,
        "chordFacet(\"a\\\"\",\"digest\",function(){}\n);\n"
    );
    let preferences = Preferences::default();
    let principal = serde_json::json!("user");
    preferences.choose(&principal, JsonValue::String("dark".into()))?;
    assert_eq!(
        preferences.pick(&principal, JsonValue::Null)?,
        JsonValue::String("dark".into())
    );
    Ok(())
}
#[tokio::test]
async fn terminal_scopes_release_watches_keep_page_state_and_retain_optimism_until_all_calls_settle()
-> pi_rs_chord::Result<()> {
    use pi_rs_chord::{
        context::Context,
        host::*,
        surfaces::*,
        tui::{TerminalTheme, TuiScope},
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let host = create_host(HostOptions::default())?;
    let redraws = Arc::new(AtomicUsize::new(0));
    let changed = redraws.clone();
    let scope = TuiScope::new(host.clone(), move || {
        changed.fetch_add(1, Ordering::SeqCst);
    });
    let page = scope.page_state("panel", 1_i32)?;
    page.set(2, &Context::background())?;
    let base = Arc::new(AtomicUsize::new(10));
    let read = base.clone();
    let optimistic = Arc::new(scope.optimistic(
        move || read.load(Ordering::SeqCst),
        |value, action: &usize| value + action,
    ));
    let (finish_a, wait_a) = tokio::sync::oneshot::channel::<()>();
    let (finish_b, wait_b) = tokio::sync::oneshot::channel::<()>();
    let a = optimistic.clone();
    let first = tokio::spawn(async move { a.add(1, wait_a).await });
    tokio::task::yield_now().await;
    let b = optimistic.clone();
    let second = tokio::spawn(async move { b.add(2, wait_b).await });
    tokio::task::yield_now().await;
    assert!(optimistic.pending());
    assert_eq!(optimistic.read(), 13);
    assert!(finish_a.send(()).is_ok());
    assert!(first.await.unwrap().is_ok());
    assert_eq!(optimistic.read(), 13);
    base.store(14, Ordering::SeqCst);
    assert!(finish_b.send(()).is_ok());
    assert!(second.await.unwrap().is_ok());
    assert!(!optimistic.pending());
    assert_eq!(optimistic.read(), 14);
    scope.dispose();
    let before = redraws.load(Ordering::SeqCst);
    page.set(3, &Context::background())?;
    assert_eq!(redraws.load(Ordering::SeqCst), before);
    let reopened = TuiScope::new(host.clone(), || {});
    assert_eq!(reopened.page_state("panel", 99_i32)?.value(), 3);
    let theme = TerminalTheme::new(fallback_theme(ThemeMode::Dark), ColorDepth::Truecolor);
    assert_eq!(
        theme.fg("accent", "text"),
        "\x1b[38;2;76;154;255mtext\x1b[39m"
    );
    assert_eq!(theme.bg("unknown", "text"), "\x1b[49mtext\x1b[49m");
    reopened.dispose();
    host.close(&Context::background()).await?;
    Ok(())
}
