//! `agent-plugins` built-in plugin: contributes the `p2-agent-plugins` prompt hint
//! for conversations running on remote environments and registers the built-in plugin skills dir.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use pi_rs_chord::{
    Result,
    contracts::Contract,
    host::{Members, ServeOptions, define_facet},
};
use pi_rs_durable::harness::{Extension, define_extension, section};
use serde_json::Value;

use crate::{
    api::v1::{FacetKind, PluginRegistry, node::user_plugins_directory, worker::worker_of},
    plugins::prompt::contract::get_or_install_plugin_skills,
};

const BUNDLED_SKILLS: &[(&str, &str)] = &[
    (
        "writing-p2-plugins/SKILL.md",
        include_str!("../../../assets/agent-plugins/skills/writing-p2-plugins/SKILL.md"),
    ),
    (
        "writing-p2-plugins/example/counter/contract.ts",
        include_str!(
            "../../../assets/agent-plugins/skills/writing-p2-plugins/example/counter/contract.ts"
        ),
    ),
    (
        "writing-p2-plugins/example/counter/package.json",
        include_str!(
            "../../../assets/agent-plugins/skills/writing-p2-plugins/example/counter/package.json"
        ),
    ),
    (
        "writing-p2-plugins/example/counter/server.ts",
        include_str!(
            "../../../assets/agent-plugins/skills/writing-p2-plugins/example/counter/server.ts"
        ),
    ),
    (
        "writing-p2-plugins/example/counter/web.tsx",
        include_str!(
            "../../../assets/agent-plugins/skills/writing-p2-plugins/example/counter/web.tsx"
        ),
    ),
    (
        "writing-p2-plugins/example/counter/worker.ts",
        include_str!(
            "../../../assets/agent-plugins/skills/writing-p2-plugins/example/counter/worker.ts"
        ),
    ),
];

/// Writes the bundled `writing-p2-plugins` skill into `<data_dir>/bundled-skills/agent-plugins`
/// and returns that skills root directory for `PluginSkillsService`.
pub fn materialize_bundled_skills(data_dir: &Path) -> Result<PathBuf> {
    let root = data_dir.join("bundled-skills").join("agent-plugins");
    for (rel_path, contents) in BUNDLED_SKILLS {
        let dest = root.join(rel_path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let unchanged = std::fs::read_to_string(&dest).is_ok_and(|existing| existing == *contents);
        if !unchanged {
            std::fs::write(&dest, contents)?;
        }
    }
    Ok(root)
}

/// The hint a conversation on another environment gets, for the user plugin root `root`.
pub fn remote_hint(root: &str) -> String {
    let quoted = serde_json::to_string(root).unwrap_or_else(|_| format!("\"{root}\""));
    format!(
        "Multiplayer or persistent views (games, shared boards, editors several people use) are p2 plugins on the p2 server. To write one, spawn a subagent with env null and cwd {quoted} (location local:{root}); its skills include writing-p2-plugins."
    )
}

pub fn create_agent_plugins_extension(
    data_dir: String,
    current_env: Arc<dyn Fn() -> Option<String> + Send + Sync>,
) -> Extension {
    let root = user_plugins_directory(std::path::Path::new(&data_dir))
        .to_string_lossy()
        .into_owned();
    define_extension(Extension {
        name: "p2-agent-plugins".into(),
        sections: vec![section(
            "p2-plugins",
            Arc::new(move |input: &Value, _| {
                let root = root.clone();
                let env_from_input = input
                    .get("env")
                    .and_then(|e| e.get("name"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let is_remote = env_from_input.is_some() || current_env().is_some();
                Box::pin(async move {
                    if is_remote {
                        Ok(Some(remote_hint(&root)))
                    } else {
                        Ok(None)
                    }
                })
            }),
            None,
        )],
        ..Default::default()
    })
}

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    let local_contract = Arc::new(Contract::local(
        "p2.agent-plugins",
        1,
        Vec::new(),
        indexmap::IndexMap::new(),
    )?);
    registry.register(
        "agent-plugins",
        FacetKind::Worker,
        define_facet(move |host| {
            host.serve(
                local_contract.clone(),
                move |_, _, object| {
                    if let Ok(worker) = worker_of(&object.host()) {
                        let plugin_skills = get_or_install_plugin_skills(&object.host())?;
                        let skills_dir = materialize_bundled_skills(&worker.init.data_dir)?;
                        plugin_skills
                            .contribute("agent-plugins", skills_dir.to_string_lossy().into_owned());
                        let loc = worker.location.clone();
                        let extension = create_agent_plugins_extension(
                            worker.init.data_dir.to_string_lossy().into_owned(),
                            Arc::new(move || loc.value().env),
                        );
                        let ext_name = extension.name.clone();
                        let registries = worker.registries.clone();
                        registries.extensions.install(extension)?;
                        let cleanup_skills = plugin_skills.clone();
                        let _cleanup = object.open(
                            |_| Box::pin(async { Ok(()) }),
                            move |_, _| {
                                let registries = registries.clone();
                                let ext_name = ext_name.clone();
                                let cleanup_skills = cleanup_skills.clone();
                                Box::pin(async move {
                                    cleanup_skills.remove("agent-plugins");
                                    let _ = registries.extensions.uninstall(&ext_name);
                                    Ok(())
                                })
                            },
                        );
                    }
                    Ok(Members::new())
                },
                ServeOptions::default(),
            )
        }),
    )
}
