use std::{collections::BTreeMap, error::Error, sync::Arc};

use indexmap::IndexMap;
use pi_rs_ai::{
    ThinkingLevel, Tool,
    faux::{FauxModelDefinition, FauxOptions, faux_provider},
};
use pi_rs_chord::{context::Context, host::Caller, json::JsonValue};
use pi_rs_durable::harness::{Agent, TaskGraph, TaskGraphNode, ToolRegistration};
use pi_rs_p2::{
    api::v1::{FacetKind, GrantMode, OwnerAccess, PluginRegistry, Principal},
    hosts::server::{ServerOptions, start_server},
    plugins::{
        register_builtins,
        subagents::{
            children::child_rows_history,
            contract::{
                Child, ChildFailure, by_recent_activity, child_store_contract,
                conversation_location, model_ref_of, remote_environments, short_model_name,
                strip_children_map_v3, subagent_control_contract, subagent_sender_text,
                subagents_contract,
            },
            contract_worker::{
                AgentChoice, AgentKind, Subagent, ToolScope, choosable_tools, resolve_choice,
                resolve_tools, still_given,
            },
            model_list::{ListedModel, close_models, list_models},
            worker::{FailureSource, Seen, failure_source, working_children},
        },
    },
    runtime::location::Location,
};
use serde_json::json;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

fn dummy_tool(name: &str, callers: Option<Vec<&str>>, deferred: bool) -> ToolRegistration {
    ToolRegistration {
        declaration: Tool {
            name: name.into(),
            description: format!("{name} tool"),
            parameters: json!({ "type": "object", "properties": {} }),
            constrained_sampling: None,
        },
        sequential: false,
        prepare_arguments: None,
        cancel: None,
        structured_output_schema: None,
        output_limits: None,
        replay_safe: true,
        callers: callers.map(|c| c.into_iter().map(str::to_owned).collect()),
        deferred,
        execute: Arc::new(|_, _, _| Box::pin(async { unreachable!() })),
    }
}

#[test]
fn subagents_contracts_and_helpers() -> TestResult {
    let mut registry = PluginRegistry::new();
    register_builtins(&mut registry)?;
    let selected = registry.select(None)?;
    assert!(selected.contains(&"subagents".to_owned()));
    assert!(selected.contains(&"voice".to_owned()));
    assert_eq!(
        registry
            .facets(FacetKind::Server, &["subagents".into()])
            .len(),
        1
    );
    assert_eq!(
        registry
            .facets(FacetKind::Worker, &["subagents".into()])
            .len(),
        1
    );

    assert_eq!(child_store_contract()?.name(), "p2.child-store");
    assert_eq!(subagents_contract()?.name(), "p2.subagents");
    assert_eq!(subagent_control_contract()?.name(), "p2.subagent-control");
    assert_eq!(child_rows_history()?.definition.version, 2);

    assert_eq!(
        model_ref_of("anthropic/claude-sonnet-4"),
        Some(("anthropic", "claude-sonnet-4"))
    );
    assert_eq!(
        model_ref_of("openrouter/org/model"),
        Some(("openrouter", "org/model"))
    );
    assert_eq!(model_ref_of("noslash"), None);
    assert_eq!(model_ref_of("/leading"), None);
    assert_eq!(short_model_name("openrouter/org/model"), "model");
    assert_eq!(
        subagent_sender_text("a\"b<c>&"),
        "<sender kind=\"subagent\" name=\"a&quot;b&lt;c&gt;&amp;\"/>"
    );

    let c1 = Child {
        name: "alpha".into(),
        kind: None,
        call_id: None,
        archived: None,
        conversation_id: 2,
        model: "p/m".into(),
        thinking_level: "off".into(),
        tools: vec![],
        location: Some(Location {
            cwd: "/wt".into(),
            env: Some("box-a".into()),
        }),
        last_entry_id: 10,
        updated_at: Some(1234),
        failure: Some(ChildFailure {
            reason: "aborted".into(),
            detail: Some("secret".into()),
        }),
    };
    let c2 = Child {
        name: "beta".into(),
        conversation_id: 3,
        last_entry_id: 20,
        location: None,
        ..c1.clone()
    };
    assert!(by_recent_activity(&c2, &c1).is_lt());
    assert_eq!(
        conversation_location(Some("/sess"), Some("main"), Some(&c1)).map(|l| l.cwd),
        Some("/wt".into())
    );
    assert_eq!(
        conversation_location(Some("/sess"), Some("main"), Some(&c2)),
        Some(Location {
            cwd: "/sess".into(),
            env: Some("main".into()),
        })
    );
    assert_eq!(
        remote_environments(Some("main"), &[c1.clone(), c2]),
        vec!["main".to_owned(), "box-a".to_owned()]
    );

    let stripped = strip_children_map_v3(json!({ "s1": [c1] }));
    assert!(stripped["s1"][0].get("updatedAt").is_none());
    assert!(stripped["s1"][0].get("failure").is_none());
    Ok(())
}

#[test]
fn subagents_model_list_and_close_matches() {
    assert_eq!(
        list_models(&[], None),
        "No models with configured credentials."
    );
    assert_eq!(
        list_models(&[], Some("sonnet")),
        "No available model matches sonnet."
    );

    let small = vec![
        ListedModel {
            provider: "anthropic".into(),
            id: "claude-fable-5-2".into(),
            name: "Claude Fable 5.2".into(),
        },
        ListedModel {
            provider: "openai".into(),
            id: "fable-5-2".into(),
            name: "Fable 5.2".into(),
        },
    ];
    assert_eq!(
        list_models(&small, Some("5 2")),
        "anthropic/claude-fable-5-2 (Claude Fable 5.2)\nopenai/fable-5-2 (Fable 5.2)"
    );
    assert_eq!(
        close_models(&small, "anthropic/fable-5.2"),
        vec!["anthropic/claude-fable-5-2"]
    );
    assert_eq!(
        close_models(&small, "fable-5.2"),
        vec!["anthropic/claude-fable-5-2", "openai/fable-5-2"]
    );

    let many: Vec<ListedModel> = (0..105)
        .map(|i| ListedModel {
            provider: if i < 60 { "anthropic" } else { "openai" }.into(),
            id: format!("model-{i}"),
            name: format!("Model {i}"),
        })
        .collect();
    let grouped = list_models(&many, None);
    assert!(grouped.contains("105 models, too many to list. By provider:"));
    assert!(grouped.contains("anthropic: 60"));
    assert!(grouped.contains("openai: 45"));
}

#[tokio::test]
async fn subagents_tool_and_choice_resolution() -> TestResult {
    let caller = Agent {
        state: json!({ "model": { "provider": "anthropic", "modelId": "claude-sonnet-4" } }),
        extensions: vec![],
        tools: vec![
            dummy_tool("read", Some(vec!["model"]), false),
            dummy_tool("codemode", Some(vec!["model"]), false),
            dummy_tool("subagent", Some(vec!["model"]), false),
        ],
        deferred: vec![
            dummy_tool("team", Some(vec!["model"]), true),
            dummy_tool("web_search", Some(vec!["model"]), true),
        ],
        callable: vec![
            dummy_tool("bash", Some(vec!["script"]), false),
            dummy_tool("main_script", Some(vec!["script"]), false),
        ],
        sections: vec![],
    };

    let mut access = IndexMap::new();
    access.insert("subagent".into(), ToolScope::Main);
    access.insert("team".into(), ToolScope::Subagent);
    access.insert("main_script".into(), ToolScope::Main);

    let choosable: Vec<String> = choosable_tools(&caller, &access)
        .into_iter()
        .map(|t| t.declaration.name)
        .collect();
    assert_eq!(choosable, vec!["read", "codemode", "web_search"]);

    let resolved = resolve_tools(
        &caller,
        &access,
        Some(&["Read".into(), "codemode".into(), "web_search".into()]),
        &["web_search".into()],
        AgentKind::Subagent,
    )
    .map_err(|e| format!("unexpected error: {e}"))?;
    assert_eq!(resolved.chosen, vec!["read", "codemode", "web_search"]);
    let tool_names: Vec<&str> = resolved
        .tools
        .iter()
        .map(|t| t.declaration.name.as_str())
        .collect();
    assert_eq!(
        tool_names,
        vec!["read", "codemode", "web_search", "bash", "team"]
    );
    let loaded_names: Vec<&str> = resolved
        .loaded
        .iter()
        .map(|t| t.declaration.name.as_str())
        .collect();
    assert_eq!(loaded_names, vec!["team", "web_search"]);

    let current = Subagent {
        conversation_id: 2,
        model: "anthropic/claude-sonnet-4".into(),
        thinking_level: "high".into(),
        tools: vec!["read".into(), "removed_tool".into()],
        location: None,
        unplaced_cwd: None,
        parent: None,
        reported: vec![],
        archived: None,
    };
    assert_eq!(
        still_given(&caller, &access, Some(&current)),
        Some(vec!["read".into()])
    );

    let models = pi_rs_ai::registry::Models::default();
    let faux = faux_provider(FauxOptions {
        provider: Some("anthropic".into()),
        models: vec![FauxModelDefinition {
            id: "claude-sonnet-4".into(),
            name: Some("Claude Sonnet 4".into()),
            reasoning: false,
            ..Default::default()
        }],
        ..Default::default()
    });
    models.set_provider(faux.provider.clone());
    let ctx = Context::background();

    let err = resolve_choice(
        &models,
        &caller,
        &access,
        &AgentChoice {
            model: Some("anthropic/sonnet 4".into()),
            ..Default::default()
        },
        None,
        AgentKind::Subagent,
        &ctx,
    )
    .await;
    assert!(matches!(err, Err(msg) if msg.contains("Close matches: anthropic/claude-sonnet-4.")));

    let clamped = resolve_choice(
        &models,
        &caller,
        &access,
        &AgentChoice {
            thinking_level: Some("xhigh".into()),
            ..Default::default()
        },
        None,
        AgentKind::Subagent,
        &ctx,
    )
    .await
    .map_err(|e| format!("unexpected error: {e}"))?;
    assert_eq!(clamped.thinking_level, ThinkingLevel::Off);
    Ok(())
}

#[test]
fn subagents_failure_source_and_working_children() {
    let seen = Seen {
        last_entry_id: Some(10),
        updated_at: Some(500),
        failure: Some(ChildFailure {
            reason: "no_model".into(),
            detail: None,
        }),
    };
    assert_eq!(
        failure_source(Some(&seen), Some(10), false),
        FailureSource::Cached(seen.failure.clone())
    );
    assert_eq!(
        failure_source(Some(&seen), Some(11), false),
        FailureSource::Read
    );
    assert_eq!(
        failure_source(Some(&seen), Some(10), true),
        FailureSource::Read
    );
    assert_eq!(failure_source(None, Some(10), false), FailureSource::Read);

    let mut seen_map = BTreeMap::new();
    seen_map.insert(2, seen);
    let mut graph = TaskGraph::default();
    graph.tasks.insert(
        1,
        TaskGraphNode {
            id: 1,
            kind: "t1".into(),
            conversation_id: 2,
            owner: None,
            background: false,
            abort_requested: false,
            state: json!({}),
            blocked: None,
            conversations: vec![2],
        },
    );
    graph.tasks.insert(
        2,
        TaskGraphNode {
            id: 2,
            kind: "t2".into(),
            conversation_id: 3,
            owner: None,
            background: false,
            abort_requested: false,
            state: json!({}),
            blocked: None,
            conversations: vec![3],
        },
    );
    let working = working_children(&graph, &seen_map);
    assert_eq!(working.into_iter().collect::<Vec<_>>(), vec![2]);
}

#[tokio::test]
async fn subagents_server_facet_serves_child_store_and_strips_guest_detail() -> TestResult {
    let dir = tempfile::tempdir()?;
    let mut options = ServerOptions::new(dir.path().into());
    options.port = 0;
    register_builtins(&mut options.registry)?;
    let mut auth = pi_rs_p2::runtime::server::ServerServices::default();
    auth.authenticators.insert(
        "test-owner".into(),
        Arc::new(|req, _| {
            Box::pin(async move {
                let is_owner = req
                    .headers
                    .get(http::header::COOKIE)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|c| c.contains("fixture=owner"));
                Ok(is_owner.then_some(Principal::Owner {
                    access: OwnerAccess::Local,
                }))
            })
        }),
    );
    options
        .registry
        .register("test-auth", FacetKind::Server, auth.facet())?;
    let server = start_server(options).await?;
    let ctx = Context::background();
    let root = server.host.root();

    let child_store = root.hold(
        Arc::new(child_store_contract()?),
        JsonValue::Object(IndexMap::new()),
    )?;
    child_store.wait(&ctx).await?;

    // Report from non-running worker is ignored (returns null and writes nothing).
    let child = json!([{
        "name": "worker-a",
        "conversationId": 2,
        "model": "anthropic/claude-sonnet-4",
        "thinkingLevel": "off",
        "tools": ["read"],
        "lastEntryId": 5,
        "updatedAt": 1000,
        "failure": { "reason": "error", "detail": "/secret/path" }
    }]);
    child_store
        .call(
            "report",
            JsonValue::from_serde(json!({
                "session": "sess-1",
                "generation": 1,
                "children": child,
            })),
            &ctx,
        )
        .await?;
    assert_eq!(
        child_store
            .member("children")?
            .value()?
            .to_json()
            .to_serde()?,
        json!({})
    );

    // Owner and guest reader views on p2.subagents:
    let subagents = Arc::new(subagents_contract()?);
    let owner_obj = server.host.hold_as(
        subagents.clone(),
        JsonValue::Object(IndexMap::new()),
        Caller {
            principal: Some(serde_json::to_value(Principal::Owner {
                access: OwnerAccess::Local,
            })?),
            incarnation: uuid::Uuid::new_v4().to_string(),
        },
        &ctx,
    )?;
    owner_obj.wait(&ctx).await?;
    assert_eq!(owner_obj.variant()?, "reader");

    let mut guest_sessions = IndexMap::new();
    guest_sessions.insert("sess-1".into(), GrantMode::View);
    let guest_obj = server.host.hold_as(
        subagents,
        JsonValue::Object(IndexMap::new()),
        Caller {
            principal: Some(serde_json::to_value(Principal::Guest {
                id: "g1".into(),
                name: "Guest".into(),
                email: None,
                home: "sess-1".into(),
                sessions: guest_sessions,
                revision: 1,
            })?),
            incarnation: uuid::Uuid::new_v4().to_string(),
        },
        &ctx,
    )?;
    guest_obj.wait(&ctx).await?;
    assert_eq!(guest_obj.variant()?, "reader");
    assert_eq!(
        guest_obj
            .member("children")?
            .value()?
            .to_json()
            .to_serde()?,
        json!({ "sess-1": [] })
    );

    drop(root);
    let _ = server.close().await;
    Ok(())
}

#[test]
fn voice_protocol_frames_and_validation() {
    use pi_rs_p2::plugins::voice::protocol::{
        AGENT_AUDIO, is_valid_room_command, is_valid_room_personal, parse_room_frame, room_name,
        validate_log_batch, validate_sample, validate_voices,
    };

    // Odd ID length (3) -> 2 + 3 = 5 (odd) -> 1 pad byte -> offset 6
    let frame_odd = [AGENT_AUDIO, 3, b'a', b'b', b'c', 0, 10, 20, 30, 40];
    let parsed_odd = parse_room_frame(&frame_odd).expect("odd frame");
    assert_eq!(parsed_odd.kind, AGENT_AUDIO);
    assert_eq!(parsed_odd.id, "abc");
    assert_eq!(parsed_odd.pcm, vec![10, 20, 30, 40]);

    // Even ID length (2) -> 2 + 2 = 4 (even) -> no pad byte -> offset 4
    let frame_even = [AGENT_AUDIO, 2, b'x', b'y', 1, 2];
    let parsed_even = parse_room_frame(&frame_even).expect("even frame");
    assert_eq!(parsed_even.id, "xy");
    assert_eq!(parsed_even.pcm, vec![1, 2]);
    assert_eq!(parse_room_frame(&[1, 5, b'a']), None);

    assert_eq!(room_name("abc", 1), Some("p2-abc-1".into()));
    assert_eq!(room_name("bad/id", 1), None);

    assert!(is_valid_room_personal(&json!({ "type": "reset" })));
    assert!(is_valid_room_personal(
        &json!({ "type": "vad", "silence": 0.8 })
    ));
    assert!(!is_valid_room_personal(
        &json!({ "type": "vad", "silence": 5.0 })
    ));
    assert!(!is_valid_room_personal(
        &json!({ "type": "speak", "id": "u1", "text": "hi" })
    ));
    assert!(is_valid_room_command(
        &json!({ "type": "speak", "id": "u1", "text": "hi" })
    ));

    assert!(validate_sample(&json!({ "speed": 1.0, "engine": "kokoro" })).is_some());
    assert!(validate_sample(&json!({ "speed": 3.0 })).is_none());
    assert!(
        validate_log_batch(
            &json!({ "client": "tab1", "events": [{ "t": 1000, "message": "ok" }] })
        )
        .is_some()
    );
    assert!(validate_log_batch(&json!({ "client": "bad/client", "events": [] })).is_none());

    let voices_json = json!({
        "engines": [{
            "id": "kokoro",
            "name": "Kokoro",
            "streaming": true,
            "sampleRate": 24000,
            "voices": [{ "id": "af_heart", "name": "Heart" }],
            "languages": [{ "id": "fr", "name": "French" }, { "id": "en", "name": "English" }],
            "defaultLanguage": "en"
        }],
        "default": { "engine": "kokoro", "voice": "af_heart", "language": "en" }
    });
    assert!(validate_voices(&voices_json).is_some());
}

#[test]
fn voice_choice_and_policy() {
    use pi_rs_p2::plugins::voice::{
        choice::{VoiceSettings, engine_label, languages_of, resolve_voice, resolved},
        policy::{AudioOutput, BargeInPhase, barge_in_phase, echo_cancellation_for},
        protocol::{DefaultVoice, LanguageInfo, TtsEngine, VoiceInfo, Voices},
    };

    assert_eq!(barge_in_phase(true, false), BargeInPhase::Hearing);
    assert_eq!(barge_in_phase(false, true), BargeInPhase::Speaking);
    assert_eq!(barge_in_phase(false, false), BargeInPhase::Listening);

    let out = |kind: &str, id: &str, label: &str| AudioOutput {
        kind: kind.into(),
        device_id: id.into(),
        label: label.into(),
    };
    assert!(echo_cancellation_for(&[]));
    assert!(echo_cancellation_for(&[out(
        "audiooutput",
        "default",
        "MacBook Pro Speakers"
    )]));
    assert!(!echo_cancellation_for(&[out(
        "audiooutput",
        "default",
        "External Headphones"
    )]));
    assert!(!echo_cancellation_for(&[
        out("audiooutput", "spk", "Built-in Speaker"),
        out("audiooutput", "default", "AirPods Pro"),
    ]));
    assert!(!echo_cancellation_for(&[out(
        "audiooutput",
        "first",
        "Kopfhörer (Bluetooth)"
    )]));
    assert!(echo_cancellation_for(&[out(
        "audioinput",
        "default",
        "Headset Microphone"
    )]));

    let config = Voices {
        engines: vec![
            TtsEngine {
                id: "pocket".into(),
                name: "Pocket TTS".into(),
                streaming: true,
                sample_rate: 24000,
                voices: vec![VoiceInfo {
                    id: "alba".into(),
                    name: "Alba".into(),
                }],
                languages: Some(vec![LanguageInfo {
                    id: "en".into(),
                    name: "English".into(),
                }]),
                default_language: Some("en".into()),
            },
            TtsEngine {
                id: "kokoro".into(),
                name: "Kokoro".into(),
                streaming: false,
                sample_rate: 24000,
                voices: vec![VoiceInfo {
                    id: "heart".into(),
                    name: "Heart".into(),
                }],
                languages: Some(vec![
                    LanguageInfo {
                        id: "fr".into(),
                        name: "French".into(),
                    },
                    LanguageInfo {
                        id: "de".into(),
                        name: "German".into(),
                    },
                    LanguageInfo {
                        id: "en".into(),
                        name: "English".into(),
                    },
                ]),
                default_language: Some("en".into()),
            },
        ],
        default: DefaultVoice {
            engine: "pocket".into(),
            voice: "alba".into(),
            language: Some("en".into()),
        },
    };

    assert_eq!(engine_label(&config.engines[0]), "Pocket");
    let langs = languages_of(Some(&config.engines[1]));
    assert_eq!(
        langs.iter().map(|l| l.id.as_str()).collect::<Vec<_>>(),
        vec!["en", "de", "fr"]
    );

    let fallback = resolve_voice(
        Some(&config),
        &VoiceSettings {
            engine: "missing".into(),
            voice: "missing".into(),
            speed: 1.0,
            language: Some("ja".into()),
        },
    );
    assert_eq!(fallback.engine.map(|e| e.id.as_str()), Some("pocket"));
    assert_eq!(fallback.voice, "alba");
    assert_eq!(fallback.language.as_deref(), Some("en"));

    let switched = resolved(
        Some(&config),
        &VoiceSettings {
            engine: "pocket".into(),
            voice: "heart".into(),
            speed: 1.2,
            language: Some("de".into()),
        },
    );
    assert_eq!(switched.voice, "alba");
    assert_eq!(switched.language.as_deref(), Some("en"));
}

#[test]
fn voice_speech_text_chunking_and_stop_commands() {
    use pi_rs_p2::plugins::voice::speech_text::{SpeechChunker, is_stop_command, speakable};

    assert_eq!(
        speakable("## **Hello** [world](https://example.com)!"),
        "Hello world!"
    );
    assert_eq!(speakable("| a | b |"), "");
    assert_eq!(speakable("---"), "");
    assert_eq!(
        speakable("Visit https://example.com now"),
        "Visit a link now"
    );

    let mut chunker = SpeechChunker::new();
    let text = "Dr. Smith ran `a.out`. Then:\n1. First item.\n```rust\nlet x = 1;\n```\nDone";
    let first = chunker.push(&text[..text.len() - 4], false);
    assert_eq!(first, vec!["Dr. Smith ran a.out.", "Then:", "First item."]);
    let rest = chunker.push(text, true);
    assert_eq!(rest, vec!["Done"]);

    assert!(is_stop_command("Stop!"));
    assert!(is_stop_command("Okay, stop."));
    assert!(is_stop_command("never mind"));
    assert!(!is_stop_command("stop the server please"));
}

#[test]
fn voice_worker_prompt_and_doc() {
    use pi_rs_p2::plugins::voice::worker::{
        VOICE_PROMPT, language_name, voice_mode_doc, voice_prompt,
    };

    assert_eq!(language_name("de"), "German");
    assert_eq!(voice_prompt(None), VOICE_PROMPT);
    assert_eq!(voice_prompt(Some("en-US")), VOICE_PROMPT);
    let de_prompt = voice_prompt(Some("de"));
    assert!(de_prompt.contains("Speak German: reply in German"));

    let doc = voice_mode_doc();
    assert_eq!(doc.kind, "voice.mode");
    assert_eq!(doc.version, 2);
}

#[tokio::test]
async fn voice_server_facet_and_routes() -> TestResult {
    use pi_rs_p2::plugins::voice::{
        contract::{voice_service_contract, voice_service_store_contract},
        server::{read_config, strip_json_comments},
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    assert_eq!(
        strip_json_comments(
            "{\n// comment\n\"url\": \"https://a//b\", /* block */ \"token\": \"t\"\n}"
        ),
        "{\n\n\"url\": \"https://a//b\",  \"token\": \"t\"\n}"
    );

    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("voice.json"),
        "// config\n{\"url\": \"https://voice.example.com/\", \"token\": \"secret\"}\n",
    )?;
    let cfg = read_config(dir.path()).await.expect("valid voice.json");
    assert_eq!(cfg.url, "https://voice.example.com");
    assert_eq!(cfg.token, "secret");

    // Start server without voice.json to avoid external network calls.
    std::fs::remove_file(dir.path().join("voice.json"))?;
    let mut options = ServerOptions::new(dir.path().into());
    options.port = 0;
    register_builtins(&mut options.registry)?;
    let mut auth = pi_rs_p2::runtime::server::ServerServices::default();
    auth.authenticators.insert(
        "test-owner".into(),
        Arc::new(|req, _| {
            Box::pin(async move {
                let is_owner = req
                    .headers
                    .get(http::header::COOKIE)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|c| c.contains("fixture=owner"));
                Ok(is_owner.then_some(Principal::Owner {
                    access: OwnerAccess::Local,
                }))
            })
        }),
    );
    options
        .registry
        .register("test-auth", FacetKind::Server, auth.facet())?;
    let server = start_server(options).await?;
    let ctx = Context::background();
    let root = server.host.root();

    let store = root.hold(
        Arc::new(voice_service_store_contract()?),
        JsonValue::Object(IndexMap::new()),
    )?;
    store.wait(&ctx).await?;
    assert_eq!(
        store.member("voices")?.value()?.to_json().to_serde()?,
        json!(null)
    );

    let voice_svc = Arc::new(voice_service_contract()?);
    let owner_svc = server.host.hold_as(
        voice_svc.clone(),
        JsonValue::Object(IndexMap::new()),
        Caller {
            principal: Some(serde_json::to_value(Principal::Owner {
                access: OwnerAccess::Local,
            })?),
            incarnation: uuid::Uuid::new_v4().to_string(),
        },
        &ctx,
    )?;
    owner_svc.wait(&ctx).await?;
    assert_eq!(owner_svc.variant()?, "talker");

    let mut view_sessions = IndexMap::new();
    view_sessions.insert("s1".into(), GrantMode::View);
    let view_guest = server.host.hold_as(
        voice_svc,
        JsonValue::Object(IndexMap::new()),
        Caller {
            principal: Some(serde_json::to_value(Principal::Guest {
                id: "g-view".into(),
                name: "Viewer".into(),
                email: None,
                home: "s1".into(),
                sessions: view_sessions,
                revision: 1,
            })?),
            incarnation: uuid::Uuid::new_v4().to_string(),
        },
        &ctx,
    )?;
    assert!(view_guest.wait(&ctx).await.is_err());

    // Local owner HTTP POST /voice/log -> 204, POST /voice/sample -> 503 (voice.json missing)
    let mut conn = tokio::net::TcpStream::connect(server.address).await?;
    let log_body =
        json!({ "client": "tab1", "events": [{ "t": 1000, "message": "hello" }] }).to_string();
    let req = format!(
        "POST /voice/log HTTP/1.1\r\nHost: {}\r\nOrigin: http://{}\r\nCookie: fixture=owner\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        server.address,
        server.address,
        log_body.len(),
        log_body
    );
    conn.write_all(req.as_bytes()).await?;
    let mut resp = String::new();
    conn.read_to_string(&mut resp).await?;
    assert!(
        resp.starts_with("HTTP/1.1 204 "),
        "unexpected log response: {resp}"
    );

    let mut conn2 = tokio::net::TcpStream::connect(server.address).await?;
    let sample_body = json!({ "speed": 1.0 }).to_string();
    let req2 = format!(
        "POST /voice/sample HTTP/1.1\r\nHost: {}\r\nOrigin: http://{}\r\nCookie: fixture=owner\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        server.address,
        server.address,
        sample_body.len(),
        sample_body
    );
    conn2.write_all(req2.as_bytes()).await?;
    let mut resp2 = String::new();
    conn2.read_to_string(&mut resp2).await?;
    assert!(
        resp2.starts_with("HTTP/1.1 503 "),
        "unexpected sample response: {resp2}"
    );

    drop(root);
    let _ = server.close().await;
    Ok(())
}
