//! Prompt plugin: app guide, AGENTS.md, skills, prompt templates, and `/skill:` / `/template` expansion.

pub mod context_files;
pub mod contract;
pub mod templates;

use std::{path::Path, sync::Arc};

use pi_rs_chord::{
    Result,
    data::ChordError,
    host::{Facet, Members, ServeOptions, define_facet},
    json::JsonValue,
};
use pi_rs_durable::harness::{Extension, PromptSection};
use serde_json::{Value, json};
use tokio::sync::RwLock;

use crate::{
    api::v1::{FacetKind, PluginRegistry, worker::worker_of},
    runtime::{principals::Principal, session_lock::session_lock},
};

use context_files::{LoadedPrompt, LocalPromptFiles, PromptFiles, load_prompt_resources};
use contract::{
    SkillInfo, TemplateInfo, get_or_install_plugin_skills, prompt_local_contract,
    prompt_resources_contract,
};
use templates::expand_prompt;

pub fn app_guide(app: &str, data_dir: &str, env: Option<&str>) -> String {
    let skills = match env {
        Some(name) => format!("\"~/.pi/p2/skills\" on the remote environment \"{name}\""),
        None => serde_json::to_string(&Path::new(data_dir).join("skills").to_string_lossy())
            .unwrap_or_else(|_| "\"skills\"".to_string()),
    };
    let app_json = serde_json::to_string(app).unwrap_or_else(|_| "\"\"".to_string());
    let data_json = serde_json::to_string(data_dir).unwrap_or_else(|_| "\"\"".to_string());
    let models_json =
        serde_json::to_string(&Path::new(data_dir).join("models.json").to_string_lossy())
            .unwrap_or_else(|_| "\"\"".to_string());
    let settings_json =
        serde_json::to_string(&Path::new(data_dir).join("settings.json").to_string_lossy())
            .unwrap_or_else(|_| "\"\"".to_string());
    let agents_json =
        serde_json::to_string(&Path::new(data_dir).join("AGENTS.md").to_string_lossy())
            .unwrap_or_else(|_| "\"\"".to_string());
    let readme_json = serde_json::to_string(&Path::new(app).join("README.md").to_string_lossy())
        .unwrap_or_else(|_| "\"\"".to_string());

    format!(
        "You are running inside Pi (p2, the plugin-based Pi Mobile).\n\
Application source and documentation: {app_json} (not the session cwd). For app changes, first read docs/handoff.md and docs/p2-notes.md there; plugins live in plugins/<name>/. Read them only when working on the app itself.\n\
Private configuration/data: {data_json}. Models: {models_json}; settings: {settings_json}; global instructions: {agents_json}; skills: {skills}. Before editing, read {readme_json}'s Configuration section and plugins/models/model-config.ts or src/runtime/settings.ts under the app directory. Valid configuration changes hot-reload; in-flight requests keep their captured configuration.\n\
Keep auth.json, owner keys/tokens and session data private. Never print, publish or commit credentials."
    )
}

fn can_send(principal: Option<&Value>, session: &str) -> bool {
    principal
        .and_then(|v| serde_json::from_value::<Principal>(v.clone()).ok())
        .is_some_and(|p| p.can_send(session))
}

fn listed_of(loaded: &LoadedPrompt) -> (Vec<TemplateInfo>, Vec<SkillInfo>) {
    let templates = loaded
        .templates
        .iter()
        .map(|t| TemplateInfo {
            name: t.name.clone(),
            description: t.description.clone(),
            argument_hint: t.argument_hint.clone(),
            source: t.source,
        })
        .collect();
    let skills = loaded
        .skill_files
        .iter()
        .map(|s| SkillInfo {
            name: s.name.clone(),
            description: s.description.clone(),
        })
        .collect();
    (templates, skills)
}

async fn load_for_location(
    data_dir: &Path,
    cwd: &str,
    plugin_skill_dirs: &[String],
) -> Result<LoadedPrompt> {
    let data_str = data_dir.to_string_lossy();
    let skills_root = data_dir.join("skills").to_string_lossy().into_owned();
    let prompts_root = data_dir.join("prompts").to_string_lossy().into_owned();
    load_prompt_resources(
        &data_str,
        cwd,
        &LocalPromptFiles,
        &skills_root,
        &prompts_root,
        plugin_skill_dirs,
    )
    .await
}

pub fn worker_facet() -> Result<Facet> {
    let prompt_c = prompt_local_contract()?;
    let resources_c = prompt_resources_contract()?;

    Ok(define_facet(move |host| {
        let host_handle = host.host();
        let worker = worker_of(&host_handle)?;
        let plugin_skills = get_or_install_plugin_skills(&host_handle)?;
        let cache = Arc::new(RwLock::new(LoadedPrompt::default()));

        let app_dir = worker.init.app.to_string_lossy().into_owned();
        let data_dir = worker.init.data_dir.to_string_lossy().into_owned();
        let data_path = worker.init.data_dir.clone();
        let loc = worker.location.clone();
        let ps_ext = plugin_skills.clone();
        let cache_ext = cache.clone();

        let worker_weak = Arc::downgrade(&worker);
        let ext = Extension {
            name: "p2-prompt".into(),
            hooks: Vec::new(),
            wraps: Vec::new(),
            tasks: Vec::new(),
            tools: Vec::new(),
            sections: vec![
                PromptSection {
                    key: "app".into(),
                    tag: true,
                    render: Arc::new(move |_input, _ctx| {
                        let app_dir = app_dir.clone();
                        let data_dir = data_dir.clone();
                        let loc = loc.clone();
                        Box::pin(async move {
                            let env = loc.value().env;
                            Ok(Some(app_guide(&app_dir, &data_dir, env.as_deref())))
                        })
                    }),
                },
                PromptSection {
                    key: "agents-md".into(),
                    tag: false,
                    render: Arc::new({
                        let data_path = data_path.clone();
                        let loc = worker.location.clone();
                        let ps = ps_ext.clone();
                        let cache = cache_ext.clone();
                        let ww = worker_weak.clone();
                        move |_input, ctx| {
                            let data_path = data_path.clone();
                            let loc = loc.clone();
                            let ps = ps.clone();
                            let cache = cache.clone();
                            let ww = ww.clone();
                            Box::pin(async move {
                                let cwd = loc.value().cwd;
                                let loaded = load_for_location(&data_path, &cwd, &ps.dirs())
                                    .await
                                    .map_err(|e| {
                                        pi_rs_durable::session::Error::invalid(e.to_string())
                                    })?;
                                *cache.write().await = loaded.clone();
                                let lock = if let Some(w) = ww.upgrade()
                                    && let Ok(parts) = w.harness()
                                {
                                    session_lock(&parts.durable.session, ctx).await.map_err(
                                        |e| pi_rs_durable::session::Error::invalid(e.to_string()),
                                    )?
                                } else {
                                    None
                                };
                                let text = if lock.is_some_and(|l| !l.instructions) {
                                    loaded.project.agents
                                } else {
                                    loaded.agents
                                };
                                Ok((!text.is_empty()).then_some(text))
                            })
                        }
                    }),
                },
                PromptSection {
                    key: "skills".into(),
                    tag: false,
                    render: Arc::new({
                        let data_path = worker.init.data_dir.clone();
                        let loc = worker.location.clone();
                        let ps = ps_ext.clone();
                        let cache = cache_ext;
                        let ww = worker_weak;
                        move |_input, ctx| {
                            let data_path = data_path.clone();
                            let loc = loc.clone();
                            let ps = ps.clone();
                            let cache = cache.clone();
                            let ww = ww.clone();
                            Box::pin(async move {
                                let cwd = loc.value().cwd;
                                let loaded = load_for_location(&data_path, &cwd, &ps.dirs())
                                    .await
                                    .map_err(|e| {
                                        pi_rs_durable::session::Error::invalid(e.to_string())
                                    })?;
                                *cache.write().await = loaded.clone();
                                let lock = if let Some(w) = ww.upgrade()
                                    && let Ok(parts) = w.harness()
                                {
                                    session_lock(&parts.durable.session, ctx).await.map_err(
                                        |e| pi_rs_durable::session::Error::invalid(e.to_string()),
                                    )?
                                } else {
                                    None
                                };
                                let text = if lock.is_some_and(|l| !l.instructions) {
                                    loaded.project.skills
                                } else {
                                    loaded.skills
                                };
                                Ok((!text.is_empty()).then_some(text))
                            })
                        }
                    }),
                },
            ],
        };
        worker.registries.extensions.install(ext)?;

        let ps_keepalive = plugin_skills.clone();
        let data_for_local = worker.init.data_dir.clone();
        let loc_for_local = worker.location.clone();
        host.serve(
            prompt_c.clone(),
            move |_, _, _| {
                let _keep = &ps_keepalive;
                let mut members = Members::new();
                let data = data_for_local.clone();
                let loc = loc_for_local.clone();
                let ps = ps_keepalive.clone();
                members.method("expand", move |args, _| {
                    let data = data.clone();
                    let loc = loc.clone();
                    let ps = ps.clone();
                    Box::pin(async move {
                        let v = args.to_serde()?;
                        let text = v
                            .get("text")
                            .and_then(Value::as_str)
                            .ok_or_else(|| ChordError::invalid("args", "missing text", None))?;
                        let cwd = loc.value().cwd;
                        let loaded = load_for_location(&data, &cwd, &ps.dirs()).await?;
                        let expanded =
                            expand_prompt(text, &loaded.templates, &loaded.skill_files, |skill| {
                                let path = skill.path.clone();
                                async move {
                                    LocalPromptFiles.read_text(&path).await.ok().flatten()
                                }
                            })
                            .await;
                        Ok(JsonValue::from_serde(serde_json::to_value(expanded)?))
                    })
                })?;
                Ok(members)
            },
            ServeOptions::default(),
        )?;

        let prompt_for_res = prompt_c.clone();
        let data_for_res = worker.init.data_dir.clone();
        let loc_for_res = worker.location.clone();
        let ps_for_res = plugin_skills.clone();
        host.serve(
            resources_c.clone(),
            move |params, principal, object| {
                let p = params.to_serde()?;
                let session = p
                    .get("session")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ChordError::invalid("params", "missing session", None))?;
                let conversation = p
                    .get("conversation")
                    .and_then(Value::as_u64)
                    .filter(|&c| c >= 1)
                    .ok_or_else(|| ChordError::invalid("params", "missing conversation", None))?;
                if !can_send(principal, session) {
                    return Err(ChordError::new("forbidden", "participants only").into());
                }

                let prompt = object.get(
                    prompt_for_res.clone(),
                    JsonValue::Object(Default::default()),
                )?;
                let data = data_for_res.clone();
                let loc = loc_for_res.clone();
                let ps = ps_for_res.clone();

                let templates_state = object.writable(JsonValue::from_serde(json!([])))?;
                let skills_state = object.writable(JsonValue::from_serde(json!([])))?;

                let t_upd = templates_state.clone();
                let s_upd = skills_state.clone();
                object.on_live(move |ctx| {
                    let data = data.clone();
                    let loc = loc.clone();
                    let ps = ps.clone();
                    let t_upd = t_upd.clone();
                    let s_upd = s_upd.clone();
                    Box::pin(async move {
                        if let Ok(l) = load_for_location(&data, &loc.value().cwd, &ps.dirs()).await
                        {
                            let (t, s) = listed_of(&l);
                            let _ = t_upd.set(
                                JsonValue::from_serde(serde_json::to_value(t)?),
                                &ctx.context,
                            );
                            let _ = s_upd.set(
                                JsonValue::from_serde(serde_json::to_value(s)?),
                                &ctx.context,
                            );
                        }
                        Ok(())
                    })
                });

                let mut members = Members::variant("participant");
                members.state("templates", templates_state)?;
                members.state("skills", skills_state)?;
                members.method("expand", move |args, ctx| {
                    let prompt = prompt.clone();
                    Box::pin(async move {
                        let v = args.to_serde()?;
                        let text = v
                            .get("text")
                            .and_then(Value::as_str)
                            .ok_or_else(|| ChordError::invalid("args", "missing text", None))?;
                        prompt
                            .call(
                                "expand",
                                JsonValue::from_serde(json!({
                                    "text": text,
                                    "conversation": conversation,
                                })),
                                &ctx.context,
                            )
                            .await
                    })
                })?;

                Ok(members)
            },
            ServeOptions::default(),
        )?;

        Ok(())
    }))
}

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    registry.register("prompt", FacetKind::Worker, worker_facet()?)
}
