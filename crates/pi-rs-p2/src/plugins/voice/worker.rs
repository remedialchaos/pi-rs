//! Worker facet for the `voice` plugin (`p2.voice-mode`, `voice.mode` doc, and `voice` prompt section).

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use pi_rs_chord::{
    Error, Result,
    context::Context,
    data::ChordError,
    delta::Segment,
    host::{Facet, Members, ServeOptions, define_facet},
    json::JsonValue,
    watch::WatchCall,
};
use pi_rs_durable::{
    ConversationId, DocDefinition, DocToken,
    documents::{DocumentSemantics, InitialDocument, LatestFork, resolve_address},
    harness::{Extension, PromptSection},
    session::{Error as DurableError, Observer},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::watch;

use crate::{
    api::v1::{
        Principal,
        worker::{WorkerProcess, worker_of},
    },
    runtime::main_conversation::agent_conversation,
};

use super::contract::{VoiceChoice, voice_mode_contract};

pub const VOICE_PROMPT: &str = "The user is talking to you by voice: their messages are transcribed speech (expect recognition errors; read them charitably), and your text replies are read aloud sentence by sentence. Reply in short, plain spoken sentences: no Markdown, headings, lists, tables, code blocks, URLs or long file paths; say what matters and offer details on request. When you are about to use tools, first say in one short sentence what you are about to do: the user cannot see your screen, and silence while you work is confusing. A direct answer needs no announcement. Stay responsive: you are the voice, not the worker. Hand anything that takes more than a few seconds (coding, commands, research, multi-step work) to a doer subagent with the subagent tool: spawn one named doer with a complete brief, or send the next task to the existing doer. Then say in one sentence what you handed off, and end your turn. Do quick lookups yourself. When the doer reports, tell the user the outcome briefly. When the user asks how it is going, check with subagent status or answer. Steer the doer with subagent send; when the user wants it stopped or it is going wrong, stop it with subagent stop.";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VoiceModeState {
    pub enabled: bool,
    pub voice: Option<VoiceChoice>,
}

/// Durable document `voice.mode` (v2, conversation-scoped, initial on fork).
pub fn voice_mode_doc() -> DocToken {
    Arc::new(DocDefinition {
        kind: "voice.mode".into(),
        version: 2,
        semantics: DocumentSemantics::LatestConversation {
            fork: LatestFork::Initial,
        },
        initial: InitialDocument::Singleton(Arc::new(|| {
            Ok(Map::from_iter([
                ("enabled".into(), json!(false)),
                ("voice".into(), Value::Null),
            ]))
        })),
        migrate: Some(Arc::new(|value, _from| {
            let enabled = value.get("enabled").and_then(Value::as_bool) == Some(true);
            Ok(Map::from_iter([
                ("enabled".into(), json!(enabled)),
                ("voice".into(), Value::Null),
            ]))
        })),
        checkpoint_when: Arc::new(|_, _, _| true),
    })
}

/// Maps a BCP 47 / ISO 639-1 tag to its English display name when known, else returns the tag.
pub fn language_name(language: &str) -> String {
    let primary = language
        .split('-')
        .next()
        .unwrap_or(language)
        .to_ascii_lowercase();
    match primary.as_str() {
        "en" => "English",
        "de" => "German",
        "es" => "Spanish",
        "fr" => "French",
        "it" => "Italian",
        "pt" => "Portuguese",
        "nl" => "Dutch",
        "pl" => "Polish",
        "ru" => "Russian",
        "ja" => "Japanese",
        "ko" => "Korean",
        "zh" => "Chinese",
        "ar" => "Arabic",
        "hi" => "Hindi",
        "tr" => "Turkish",
        "sv" => "Swedish",
        "da" => "Danish",
        "no" | "nb" | "nn" => "Norwegian",
        "fi" => "Finnish",
        "cs" => "Czech",
        "uk" => "Ukrainian",
        _ => return language.to_owned(),
    }
    .to_owned()
}

/// Builds the `voice` system prompt section, adding a language instruction for non-English voices.
pub fn voice_prompt(language: Option<&str>) -> String {
    match language {
        Some(lang)
            if !lang.is_empty()
                && !lang
                    .split('-')
                    .next()
                    .unwrap_or("")
                    .eq_ignore_ascii_case("en") =>
        {
            let name = language_name(lang);
            format!(
                "{VOICE_PROMPT} Speak {name}: reply in {name}, since your replies are read aloud in {name}."
            )
        }
        _ => VOICE_PROMPT.to_owned(),
    }
}

#[derive(Default)]
struct GuestVoices(Mutex<BTreeMap<ConversationId, BTreeSet<String>>>);

impl GuestVoices {
    fn set(&self, conversation: ConversationId, guest: &str, on: bool) {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if on {
            map.entry(conversation)
                .or_default()
                .insert(guest.to_owned());
        } else if let Some(set) = map.get_mut(&conversation) {
            set.remove(guest);
            if set.is_empty() {
                map.remove(&conversation);
            }
        }
    }

    fn has(&self, conversation: ConversationId) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&conversation)
    }
}

fn voice_extension(worker_ref: Weak<WorkerProcess>, guests: Arc<GuestVoices>) -> Extension {
    Extension {
        name: "p2-voice".into(),
        sections: vec![PromptSection {
            key: "voice".into(),
            tag: true,
            render: Arc::new(move |input, context| {
                let worker_ref = worker_ref.clone();
                let guests = guests.clone();
                Box::pin(async move {
                    let Some(id) = input["conversationId"].as_u64() else {
                        return Ok(None);
                    };
                    let worker = worker_ref
                        .upgrade()
                        .ok_or_else(|| DurableError::invalid("Worker closed"))?;
                    let parts = worker
                        .harness()
                        .map_err(|e| DurableError::invalid(e.to_string()))?;
                    let holder = agent_conversation(&parts.durable.session, id, context)
                        .await
                        .map_err(|e| DurableError::invalid(e.to_string()))?;
                    let doc = voice_mode_doc();
                    let addr = resolve_address(&doc, Some(holder), None)
                        .map_err(|e| DurableError::invalid(e.to_string()))?;
                    let state: Option<VoiceModeState> = parts
                        .durable
                        .session
                        .snapshot(&doc, &addr, context)
                        .await?
                        .and_then(|raw| serde_json::from_value(Value::Object((*raw).clone())).ok());
                    let guest_on = guests.has(id);
                    let owner_on = state.as_ref().is_some_and(|s| s.enabled);
                    if guest_on || owner_on {
                        let lang = state
                            .as_ref()
                            .and_then(|s| s.voice.as_ref())
                            .and_then(|v| v.language.as_deref());
                        Ok(Some(voice_prompt(lang)))
                    } else {
                        Ok(None)
                    }
                })
            }),
        }],
        ..Default::default()
    }
}

fn durable_error(e: impl std::fmt::Display) -> Error {
    Error::Invalid(e.to_string())
}

/// Builds the worker facet for the `voice` plugin.
pub fn worker_facet() -> Result<Facet> {
    let contract = Arc::new(voice_mode_contract()?);
    let guests = Arc::new(GuestVoices::default());

    Ok(define_facet(move |host| {
        let host_handle = host.host();
        let process = worker_of(&host_handle)?;
        let ext = voice_extension(Arc::downgrade(&process), guests.clone());
        process.registries.extensions.install(ext)?;

        let serve_proc = process.clone();
        let serve_guests = guests.clone();
        host.serve(
            contract.clone(),
            move |params, principal_raw, self_| {
                let val = params.to_serde()?;
                let session = val
                    .get("session")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::Invalid("missing session".into()))?
                    .to_owned();
                let conversation = val
                    .get("conversation")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| Error::Invalid("missing conversation".into()))?;
                let principal: Principal = match principal_raw {
                    Some(v) => serde_json::from_value(v.clone())
                        .map_err(|e| Error::Invalid(e.to_string()))?,
                    None => {
                        return Err(ChordError::new("forbidden", "participants only").into());
                    }
                };
                if !principal.can_send(&session) {
                    return Err(ChordError::new("forbidden", "participants only").into());
                }

                let proc = serve_proc.clone();
                let doc = voice_mode_doc();
                let doc_for_follow = doc.clone();
                let proc_for_follow = proc.clone();

                let doc_source = self_.follow(move |target, ctx| {
                    let proc = proc_for_follow.clone();
                    let doc = doc_for_follow.clone();
                    Box::pin(async move {
                        let parts = proc.harness()?;
                        if parts
                            .durable
                            .conversation(conversation, &ctx.context)
                            .await
                            .map_err(durable_error)?
                            .is_none()
                        {
                            return Err(Error::Invalid(format!(
                                "Unknown conversation {conversation}"
                            )));
                        }
                        let holder =
                            agent_conversation(&parts.durable.session, conversation, &ctx.context)
                                .await
                                .map_err(durable_error)?;
                        let addr =
                            resolve_address(&doc, Some(holder), None).map_err(durable_error)?;
                        if parts
                            .durable
                            .session
                            .snapshot(&doc, &addr, &ctx.context)
                            .await
                            .map_err(durable_error)?
                            .is_none()
                        {
                            parts
                                .durable
                                .commit(
                                    async |tx| {
                                        let _ = tx.doc(&doc, &addr, None).await?;
                                        Ok(())
                                    },
                                    &ctx.context,
                                )
                                .await
                                .map_err(durable_error)?;
                        }
                        let changed = target.clone();
                        let ended = target.clone();
                        let Some(observed) = parts
                            .durable
                            .session
                            .observe_doc(
                                &doc,
                                &addr,
                                Observer::new(
                                    move |call: &WatchCall<Arc<Map<String, Value>>>, c| match call {
                                        WatchCall::Value { value, .. } => Ok(changed.set(
                                            JsonValue::from_serde(Value::Object((**value).clone())),
                                            c,
                                        )?),
                                        WatchCall::Failed { error } => {
                                            let _ = changed.fail(error, c);
                                            Ok(())
                                        }
                                    },
                                    move |_| ended.detach("ended"),
                                ),
                                &ctx.context,
                            )
                            .await
                            .map_err(durable_error)?
                        else {
                            return Err(Error::Invalid("VoiceMode document missing".into()));
                        };
                        target.set(
                            JsonValue::from_serde(Value::Object((*observed.value).clone())),
                            &ctx.context,
                        )?;
                        let sub = observed.subscription;
                        Ok(Box::new(move || drop(sub)) as Box<dyn FnOnce() + Send>)
                    })
                })?;

                let voice_doc = doc_source.clone();
                let voice_source = self_.follow(move |target, ctx| {
                    let src = voice_doc.clone();
                    Box::pin(async move {
                        let initial = src.value()?.to_json().to_serde()?;
                        target.set(
                            JsonValue::from_serde(
                                initial.get("voice").cloned().unwrap_or(Value::Null),
                            ),
                            &ctx.context,
                        )?;
                        let changed = target.clone();
                        let ended = target.clone();
                        let sub = src.subscribe(
                            move |call, c| match call {
                                WatchCall::Value { value, .. } => {
                                    let v = value.to_serde()?;
                                    changed.set(
                                        JsonValue::from_serde(
                                            v.get("voice").cloned().unwrap_or(Value::Null),
                                        ),
                                        c,
                                    )
                                }
                                WatchCall::Failed { error } => changed.fail(error, c),
                            },
                            move |_| ended.detach("ended"),
                        );
                        Ok(Box::new(move || drop(sub)) as Box<dyn FnOnce() + Send>)
                    })
                })?;

                if principal.is_owner() {
                    let enabled_src = doc_source.clone();
                    let enabled_source = self_.follow(move |target, ctx| {
                        let src = enabled_src.clone();
                        Box::pin(async move {
                            let initial = src.value()?.to_json().to_serde()?;
                            target.set(
                                JsonValue::from_serde(
                                    initial.get("enabled").cloned().unwrap_or(json!(false)),
                                ),
                                &ctx.context,
                            )?;
                            let changed = target.clone();
                            let ended = target.clone();
                            let sub = src.subscribe(
                                move |call, c| match call {
                                    WatchCall::Value { value, .. } => {
                                        let v = value.to_serde()?;
                                        changed.set(
                                            JsonValue::from_serde(
                                                v.get("enabled").cloned().unwrap_or(json!(false)),
                                            ),
                                            c,
                                        )
                                    }
                                    WatchCall::Failed { error } => changed.fail(error, c),
                                },
                                move |_| ended.detach("ended"),
                            );
                            Ok(Box::new(move || drop(sub)) as Box<dyn FnOnce() + Send>)
                        })
                    })?;

                    let mut members = Members::variant("owner");
                    members.state("enabled", enabled_source)?;
                    members.state("voice", voice_source)?;

                    let set_proc = proc.clone();
                    let set_doc = doc.clone();
                    members.method("set", move |args, ctx| {
                        let proc = set_proc.clone();
                        let doc = set_doc.clone();
                        Box::pin(async move {
                            let enabled = args
                                .to_serde()?
                                .get("enabled")
                                .and_then(Value::as_bool)
                                .ok_or_else(|| Error::Invalid("missing enabled".into()))?;
                            let parts = proc.harness()?;
                            if parts
                                .durable
                                .conversation(conversation, &ctx.context)
                                .await
                                .map_err(durable_error)?
                                .is_none()
                            {
                                return Err(ChordError::new(
                                    "invalid",
                                    format!("Unknown conversation {conversation}"),
                                )
                                .into());
                            }
                            let holder = agent_conversation(
                                &parts.durable.session,
                                conversation,
                                &ctx.context,
                            )
                            .await
                            .map_err(durable_error)?;
                            let addr =
                                resolve_address(&doc, Some(holder), None).map_err(durable_error)?;
                            parts
                                .durable
                                .commit(
                                    async |tx| {
                                        tx.doc(&doc, &addr, None).await?.set(
                                            &[Segment::Key("enabled".into())],
                                            json!(enabled).into(),
                                        )?;
                                        Ok(())
                                    },
                                    &ctx.context,
                                )
                                .await
                                .map_err(durable_error)?;
                            Ok(JsonValue::Null)
                        })
                    })?;

                    let choose_proc = proc;
                    let choose_doc = doc;
                    members.method("choose", move |args, ctx| {
                        let proc = choose_proc.clone();
                        let doc = choose_doc.clone();
                        Box::pin(async move {
                            let choice: VoiceChoice = serde_json::from_value(args.to_serde()?)
                                .map_err(|e| Error::Invalid(e.to_string()))?;
                            let parts = proc.harness()?;
                            if parts
                                .durable
                                .conversation(conversation, &ctx.context)
                                .await
                                .map_err(durable_error)?
                                .is_none()
                            {
                                return Err(ChordError::new(
                                    "invalid",
                                    format!("Unknown conversation {conversation}"),
                                )
                                .into());
                            }
                            let holder = agent_conversation(
                                &parts.durable.session,
                                conversation,
                                &ctx.context,
                            )
                            .await
                            .map_err(durable_error)?;
                            let addr =
                                resolve_address(&doc, Some(holder), None).map_err(durable_error)?;
                            let val = serde_json::to_value(&choice)?;
                            parts
                                .durable
                                .commit(
                                    async |tx| {
                                        tx.doc(&doc, &addr, None).await?.set(
                                            &[Segment::Key("voice".into())],
                                            JsonValue::from_serde(val),
                                        )?;
                                        Ok(())
                                    },
                                    &ctx.context,
                                )
                                .await
                                .map_err(durable_error)?;
                            Ok(JsonValue::Null)
                        })
                    })?;

                    Ok(members)
                } else {
                    let key = principal.key();
                    let guest_map = serve_guests.clone();
                    let enabled_state = Arc::new(Mutex::new(false));
                    let (enabled_tx, enabled_rx) = watch::channel(false);

                    let enabled_source = self_.follow(move |target, ctx| {
                        let mut rx = enabled_rx.clone();
                        Box::pin(async move {
                            let init = *rx.borrow_and_update();
                            target.set(JsonValue::Bool(init), &ctx.context)?;
                            let changed = target.clone();
                            let task = tokio::spawn(async move {
                                let bg = Context::background();
                                while rx.changed().await.is_ok() {
                                    let _ =
                                        changed.set(JsonValue::Bool(*rx.borrow_and_update()), &bg);
                                }
                            });
                            Ok(Box::new(move || task.abort()) as Box<dyn FnOnce() + Send>)
                        })
                    })?;

                    let cleanup_guests = guest_map.clone();
                    let cleanup_key = key.clone();
                    let _handle = self_.open(
                        move |_| {
                            let k = cleanup_key.clone();
                            Box::pin(async move { Ok(k) })
                        },
                        move |k, _| {
                            let guests = cleanup_guests.clone();
                            Box::pin(async move {
                                guests.set(conversation, &k, false);
                                Ok(())
                            })
                        },
                    );

                    let mut members = Members::variant("participant");
                    members.state("enabled", enabled_source)?;
                    members.state("voice", voice_source)?;
                    members.method("set", move |args, ctx| {
                        let proc = proc.clone();
                        let guests = guest_map.clone();
                        let key = key.clone();
                        let state = enabled_state.clone();
                        let tx = enabled_tx.clone();
                        Box::pin(async move {
                            let on = args
                                .to_serde()?
                                .get("enabled")
                                .and_then(Value::as_bool)
                                .ok_or_else(|| Error::Invalid("missing enabled".into()))?;
                            if on
                                && proc
                                    .harness()?
                                    .durable
                                    .conversation(conversation, &ctx.context)
                                    .await
                                    .map_err(durable_error)?
                                    .is_none()
                            {
                                return Err(ChordError::new(
                                    "invalid",
                                    format!("Unknown conversation {conversation}"),
                                )
                                .into());
                            }
                            guests.set(conversation, &key, on);
                            *state.lock().unwrap_or_else(|e| e.into_inner()) = on;
                            tx.send_replace(on);
                            Ok(JsonValue::Null)
                        })
                    })?;

                    Ok(members)
                }
            },
            ServeOptions {
                linger: Duration::from_secs(30),
                ..Default::default()
            },
        )?;

        Ok(())
    }))
}
