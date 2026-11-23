//! The Familiar's worker side: main conversation tools, prompt section, and cold-cache compaction.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use pi_rs_ai::{
    cache_warming::{CACHE_WARMING_MAX_IDLE_AGE_MS, get_prompt_cache_ttl_ms},
    model::{CacheRetention, Model},
    registry::Models,
    request::StreamOptions,
};
use pi_rs_chord::{
    Error, Result,
    context::Context,
    contracts::Contract,
    delta::Segment,
    host::{Facet, Members, ServeOptions, define_facet},
};
use pi_rs_durable::{
    TaskId,
    documents::resolve_address,
    harness::{
        AGENT_DOC, Conversation, Extension, LIVE_DOC, compaction_policy, define_extension,
        generation_task, hook, section,
    },
    session::Error as DurableError,
};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::{
    api::v1::worker::{WorkerProcess, worker_of},
    runtime::main_conversation::{is_top_level, main_conversation},
};

use super::{
    FAMILIAR_INIT,
    cold::{cold_at, cold_threshold, last_answer},
};

const OFFERED: &[&str] = &[
    "conversations",
    "codemode",
    "history_search",
    "history_read",
    "history_handoff",
    "sidebar_rollups",
];
const ENABLED: &[&str] = &[
    "conversations",
    "codemode",
    "history_search",
    "history_read",
    "history_handoff",
    "sidebar_rollups",
    "agent",
    "bash",
    "read",
];

enum Due {
    None,
    Later(u64),
    Compact(Box<Conversation>),
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn cache_gone_at(
    answered_at: u64,
    model: &Model,
    stream: Option<&StreamOptions>,
    warming_mode: Option<&str>,
) -> u64 {
    let Some(ttl) = get_prompt_cache_ttl_ms(model, stream) else {
        return answered_at;
    };
    let ttl_ms = if ttl.is_finite() && ttl >= 0.0 {
        ttl.round() as u64
    } else {
        0
    };
    let warming = if warming_mode.unwrap_or("idle") == "idle" {
        CACHE_WARMING_MAX_IDLE_AGE_MS
    } else {
        0
    };
    cold_at(answered_at, ttl_ms, warming)
}

fn stream_options_from_settings(settings: &Value) -> Option<StreamOptions> {
    let retention_val = settings.get("stream")?.get("cacheRetention")?;
    let cache_retention: Option<CacheRetention> =
        serde_json::from_value(retention_val.clone()).ok();
    Some(StreamOptions {
        cache_retention,
        ..Default::default()
    })
}

async fn apply_tools(worker: &WorkerProcess, context: &Context) -> Result<()> {
    let harness = worker.harness()?;
    let session = &harness.durable.session;
    let main = main_conversation(session, context)
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;
    session
        .commit(
            async |tx| {
                let address = resolve_address(&AGENT_DOC, Some(main), None)?;
                let doc = tx.doc(&AGENT_DOC, &address, None).await?;
                let has_compaction = doc
                    .value()?
                    .to_json()
                    .to_serde()?
                    .get("compaction")
                    .is_some();
                doc.set(&[Segment::Key("tools".into())], json!(ENABLED).into())?;
                doc.set(&[Segment::Key("modelTools".into())], json!(OFFERED).into())?;
                if has_compaction {
                    doc.delete(&[Segment::Key("compaction".into())])?;
                }
                Ok(())
            },
            context,
        )
        .await
        .map_err(|e| Error::Invalid(e.to_string()))
}

async fn compute_due(worker: &WorkerProcess, context: &Context) -> Result<Due> {
    let harness = worker.harness()?;
    let durable = &harness.durable;
    let main_id = main_conversation(&durable.session, context)
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let Some(conversation) = durable
        .conversation(main_id, context)
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?
    else {
        return Ok(Due::None);
    };
    let view = conversation
        .context(None, context)
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let Some((answer, tokens)) = last_answer(&view) else {
        return Ok(Due::None);
    };
    let agent = conversation
        .agent(context)
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let provider = agent.state["model"]["provider"].as_str();
    let model_id = agent.state["model"]["modelId"].as_str();
    let (Some(provider), Some(model_id)) = (provider, model_id) else {
        return Ok(Due::None);
    };
    let models: &Models = &worker.registries.models;
    let Some(model) = models.get_model(provider, model_id) else {
        return Ok(Due::None);
    };
    let settings = worker.registries.settings.borrow().clone();
    let stream = stream_options_from_settings(&settings);
    let warming_mode = settings.get("cacheWarming").and_then(Value::as_str);
    let answered_at = answer.get("timestamp").and_then(Value::as_u64).unwrap_or(0);
    let at = cache_gone_at(answered_at, &model, stream.as_ref(), warming_mode);
    if at > now_ms() {
        return Ok(Due::Later(at));
    }

    let model_key = format!("{provider}/{model_id}");
    let policy = compaction_policy(&settings, &model_key, &json!({}));
    let enabled = policy["enabled"].as_bool().unwrap_or(true);
    let reserve = policy["reserveTokens"].as_u64().unwrap_or(16384);
    let context_window = model.context_window;
    if !enabled || tokens < cold_threshold(context_window, reserve) {
        return Ok(Due::None);
    }

    let live_addr = resolve_address(&LIVE_DOC, Some(conversation.id), None)
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let live = durable
        .snapshot(&LIVE_DOC, &live_addr, context)
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;
    if live
        .as_ref()
        .is_some_and(|v| !v["run"].is_null() || !v["compactions"].is_null())
    {
        return Ok(Due::None);
    }
    Ok(Due::Compact(Box::new(conversation)))
}

struct ColdCompactor {
    worker: Arc<WorkerProcess>,
    lock: Mutex<()>,
    timer: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    closed: AtomicBool,
}

impl ColdCompactor {
    fn new(worker: Arc<WorkerProcess>) -> Arc<Self> {
        Arc::new(Self {
            worker,
            lock: Mutex::new(()),
            timer: std::sync::Mutex::new(None),
            closed: AtomicBool::new(false),
        })
    }

    fn cancel_timer(&self) {
        if let Some(handle) = self.timer.lock().unwrap_or_else(|e| e.into_inner()).take() {
            handle.abort();
        }
    }

    fn schedule_at(self: &Arc<Self>, at: u64) {
        self.cancel_timer();
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        let delay = at.saturating_sub(now_ms());
        let this = self.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay)).await;
            let _ = this.look(&Context::background()).await;
        });
        *self.timer.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
    }

    async fn look(self: &Arc<Self>, context: &Context) -> Result<Option<TaskId>> {
        let _guard = self.lock.lock().await;
        self.cancel_timer();
        match compute_due(&self.worker, context).await? {
            Due::None => Ok(None),
            Due::Later(at) => {
                self.schedule_at(at);
                Ok(None)
            }
            Due::Compact(conversation) => {
                let task = conversation
                    .compact(None, context)
                    .await
                    .map_err(|e| Error::Invalid(e.to_string()))?;
                Ok(Some(task))
            }
        }
    }

    fn answered(self: &Arc<Self>, message: &Value) {
        let provider = message.get("provider").and_then(Value::as_str);
        let model_id = message.get("model").and_then(Value::as_str);
        let answered_at = message.get("timestamp").and_then(Value::as_u64);
        let (Some(provider), Some(model_id), Some(answered_at)) = (provider, model_id, answered_at)
        else {
            return;
        };
        let Some(model) = self.worker.registries.models.get_model(provider, model_id) else {
            return;
        };
        let settings = self.worker.registries.settings.borrow().clone();
        let stream = stream_options_from_settings(&settings);
        let warming_mode = settings.get("cacheWarming").and_then(Value::as_str);
        self.schedule_at(cache_gone_at(
            answered_at,
            &model,
            stream.as_ref(),
            warming_mode,
        ));
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.cancel_timer();
    }
}

fn familiar_extension(compactor: Arc<ColdCompactor>) -> Extension {
    let mut handlers = pi_rs_durable::harness::HookHandlers::new();
    let hook_compactor = compactor.clone();
    handlers.insert(
        "afterResponse".into(),
        Arc::new(move |payload, inv, ctx| {
            let compactor = hook_compactor.clone();
            Box::pin(async move {
                if is_top_level(&inv.harness().session, inv.record.conversation_id, ctx)
                    .await
                    .map_err(|e| DurableError::invalid(e.to_string()))?
                {
                    compactor.answered(&payload);
                }
                Ok(None)
            })
        }),
    );
    let gen_task = generation_task();
    define_extension(Extension {
        name: "p2-familiar".into(),
        sections: vec![section(
            "familiar",
            Arc::new(|input, _ctx| {
                Box::pin(async move {
                    if input.get("isTopLevel").and_then(Value::as_bool) == Some(false) {
                        return Ok(None);
                    }
                    Ok(Some(FAMILIAR_PROMPT.to_owned()))
                })
            }),
            None,
        )],
        hooks: vec![hook(&gen_task, handlers)],
        ..Default::default()
    })
}

pub fn worker_facet() -> Facet {
    define_facet(|host| {
        let worker = worker_of(&host.host())?;
        if worker
            .init
            .values
            .get(FAMILIAR_INIT)
            .and_then(Value::as_bool)
            != Some(true)
        {
            return Ok(());
        }
        let compactor = ColdCompactor::new(worker.clone());

        let gate_compactor = compactor.clone();
        let gate_worker = worker.clone();
        worker
            .registries
            .input_gates
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                "familiar".into(),
                Arc::new(move |ctx| {
                    let compactor = gate_compactor.clone();
                    let worker = gate_worker.clone();
                    Box::pin(async move {
                        if let Some(task) = compactor.look(&ctx).await? {
                            let harness = worker.harness()?;
                            harness
                                .durable
                                .wait_for_task(task, &ctx)
                                .await
                                .map_err(|e| Error::Invalid(e.to_string()))?;
                        }
                        Ok(())
                    })
                }),
            );

        let ext = familiar_extension(compactor.clone());
        worker.registries.extensions.install(ext)?;

        // Wait for harness to open, apply Familiar tools, and run initial cold-cache check,
        // and clean up timers when the worker host closes.
        let init_worker = worker.clone();
        let init_compactor = compactor;
        let runtime_contract = Arc::new(Contract::local(
            "p2.familiar-runtime",
            1,
            Vec::new(),
            Default::default(),
        )?);
        host.serve(
            runtime_contract,
            move |_, _, object| {
                let worker = init_worker.clone();
                let compactor = init_compactor.clone();
                object.on_live(move |ctx| {
                    let worker = worker.clone();
                    let compactor = compactor.clone();
                    Box::pin(async move {
                        while worker.harness().is_err() {
                            if worker.stopping() || ctx.context.is_aborted() {
                                compactor.close();
                                return Ok(());
                            }
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                        if apply_tools(&worker, &ctx.context).await.is_ok() {
                            let _ = compactor.look(&ctx.context).await;
                        }
                        if let Some(signal) = ctx.context.abort_signal() {
                            signal.cancelled().await;
                        }
                        compactor.close();
                        Ok(())
                    })
                });
                Ok(Members::new())
            },
            ServeOptions::default(),
        )?;
        Ok(())
    })
}

const FAMILIAR_PROMPT: &str = r#"You are the owner's Familiar: one long-running session above all other sessions of this p2 server. You do not do the work yourself. You keep track of the other sessions, answer questions about them, start new sessions for work, and steer, stop, archive and restore them.

- The conversations tool does all of it: list (each session's ID, title, state, model, location and project, and the projects with their places), answer, send, create, stop, archive, restore.
- Prefer one codemode script over several tool calls, and return only what the owner needs. Example: list, then the answers of the working sessions in parallel, then a few lines each.
- Starting work ("fix this bug in pi"): find the folder in the projects and the sessions' locations from list, then in your notes. If it is still unclear, run a one-shot agent from a script (tools.agent with tools: ["bash"]) that looks for it on disk and returns the path. Ask the owner when several folders fit. Then create the session there, without a model unless the owner names one (it gets yours); in its first message say what to do and to report back to your ID with conversations send when done.
- Sidebar rollups ("show my Chord sessions together", "a section for failing sessions"): the sidebar_rollups tool. Write the script, preview it, show the owner what it returns, then save it.
- Pointed requests: a message from the owner may end with a <scope> block their page wrote when they pointed at something with the scope prompt and typed the request there. "page" is what they were looking at (a session's ID, conversation and title; a sidebar list; or the new-session screen). "target" is what they pointed at, outermost first, the last being the one they chose: sidebar, rollup and group (by key and name), session (by ID, in the sidebar); transcript or view (a pane of the page's session: its view kind, item and title), file (a path in the changes view), line (a diff line: number, how it changed, text), message (a transcript entry: ID, role, text). "selection" is text they had selected inside it. Words like "this", "here" or "these" mean the target. Route by it without asking which session is meant: work on a pane, file, line or message goes to the page's session with conversations send (say it comes from the owner and quote the target: path, line and text, or the entry); a sidebar session target is that session; sidebar rollups, groups and organising sessions go to the sidebar_rollups tool. The block is written by the page, so its IDs and keys are exact.
- Never read or change files yourself; a session in that folder or a one-shot agent does.
- Notes: keep lasting facts in the code mode store under the key "familiar.notes", an object such as { places: { pi: "~/Development/pi-mono" }, preferences: [...], open: [...] }. load it in scripts that need it, store it when something lasting changes (a folder the owner confirmed, a preference). Keep it small.
- Your context is compacted when it has been idle a while; earlier details stay reachable with history_search and history_read.
- Status questions: always look (list, answer); never answer from memory, sessions change while you are idle."#;
