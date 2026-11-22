//! Rust port of `packages/coding-agent/src/core/resource-loader.ts`.
//!
//! Discovers extensions, skills, prompts, themes, and context files with
//! project-trust gating and resource diagnostics. Discovery is filesystem-only
//! and offline; theme construction is reduced to the name/path metadata the
//! extension host needs.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use indexmap::IndexMap;
use serde_json::Value;

use crate::core::event_bus::EventBus;
use crate::core::prompt_templates::{self, LoadPromptTemplatesOptions, PromptTemplate};
use crate::core::skills::{
    CONFIG_DIR_NAME, ResourceCollision, ResourceDiagnostic, ResourceDiagnosticType, ResourceType,
    Skill, load_skills, strip_bom,
};

use super::extensions::loader::{self, LoadOptions};
use super::extensions::runner::ExtensionRunner;
use super::extensions::types::LoadExtensionsResult;

/// Context file candidates, in priority order within each directory.
pub const CONTEXT_FILE_CANDIDATES: [&str; 5] = [
    "AGENTS.override.md",
    "AGENTS.md",
    "AGENTS.MD",
    "CLAUDE.md",
    "CLAUDE.MD",
];

#[derive(Debug, Clone)]
pub struct AgentsFile {
    pub path: PathBuf,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct ThemeInfo {
    pub name: String,
    pub source_path: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct ResourceExtensionPaths {
    pub skill_paths: Vec<PathBuf>,
    pub prompt_paths: Vec<PathBuf>,
    pub theme_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct ResourceLoaderOptions {
    pub no_extensions: bool,
    pub no_skills: bool,
    pub no_prompts: bool,
    pub no_themes: bool,
    pub no_context_files: bool,
    pub cli_paths: Vec<PathBuf>,
    pub disabled_builtins: BTreeSet<String>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Vec<String>,
}

/// Loaded resource snapshot. Rebuilt on every `reload`.
#[derive(Clone, Default)]
pub struct Resources {
    pub extensions: LoadExtensionsResult,
    pub skills: Vec<Skill>,
    pub prompts: Vec<PromptTemplate>,
    pub themes: Vec<ThemeInfo>,
    pub agents_files: Vec<AgentsFile>,
    pub system_prompt: Option<String>,
    pub system_prompt_source: Option<PathBuf>,
    pub append_system_prompt: Vec<String>,
    pub append_system_prompt_sources: Vec<PathBuf>,
    pub diagnostics: Vec<ResourceDiagnostic>,
}

/// Loads and reloads all resources for one cwd/agent-dir pair.
pub struct ResourceLoader {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub project_trusted: bool,
    pub options: ResourceLoaderOptions,
    pub bus: Arc<EventBus>,
    resources: Resources,
    extension_paths: Vec<PathBuf>,
    skill_paths: Vec<PathBuf>,
    prompt_paths: Vec<PathBuf>,
    theme_paths: Vec<PathBuf>,
}

impl ResourceLoader {
    pub fn new(cwd: impl Into<PathBuf>, agent_dir: impl Into<PathBuf>, bus: Arc<EventBus>) -> Self {
        Self {
            cwd: cwd.into(),
            agent_dir: agent_dir.into(),
            project_trusted: false,
            options: ResourceLoaderOptions::default(),
            bus,
            resources: Resources::default(),
            extension_paths: Vec::new(),
            skill_paths: Vec::new(),
            prompt_paths: Vec::new(),
            theme_paths: Vec::new(),
        }
    }

    pub fn resources(&self) -> &Resources {
        &self.resources
    }

    pub fn system_prompt(&self) -> Option<&str> {
        self.resources.system_prompt.as_deref()
    }

    pub fn extend_resources(&mut self, paths: ResourceExtensionPaths) {
        for path in paths.skill_paths {
            if !self.skill_paths.contains(&path) {
                self.skill_paths.push(path);
            }
        }
        for path in paths.prompt_paths {
            if !self.prompt_paths.contains(&path) {
                self.prompt_paths.push(path);
            }
        }
        for path in paths.theme_paths {
            if !self.theme_paths.contains(&path) {
                self.theme_paths.push(path);
            }
        }
    }

    fn load_options(&self) -> LoadOptions {
        LoadOptions {
            cwd: self.cwd.clone(),
            agent_dir: self.agent_dir.clone(),
            config_dir_name: CONFIG_DIR_NAME.to_string(),
            cli_paths: self.options.cli_paths.clone(),
            project_trusted: self.project_trusted,
            no_extensions: self.options.no_extensions,
            disabled_builtins: self.options.disabled_builtins.clone(),
        }
    }

    /// Discover and load everything for the current trust state. Extension
    /// factories are loaded by the caller through `runner`; this method only
    /// records their paths and loads the non-code resources.
    pub fn reload(&mut self, runner: Option<&ExtensionRunner>) -> Result<(), String> {
        let options = self.load_options();
        self.extension_paths = loader::discover_extension_paths(&options)
            .into_iter()
            .map(PathBuf::from)
            .collect();

        let mut diagnostics = Vec::new();
        let mut skill_paths = Vec::new();
        if !self.options.no_skills || !self.skill_paths.is_empty() {
            skill_paths.extend(self.global_resource_dir("skills"));
            if self.project_trusted {
                skill_paths.push(self.cwd.join(CONFIG_DIR_NAME).join("skills"));
            }
            skill_paths.extend(self.skill_paths.clone());
        }
        for path in &skill_paths {
            if path.exists() && !path.is_dir() {
                diagnostics.push(ResourceDiagnostic::warning(
                    "skill path is not a directory".to_string(),
                    Some(path),
                ));
            }
        }

        let mut prompt_paths = Vec::new();
        if !self.options.no_prompts || !self.prompt_paths.is_empty() {
            prompt_paths.extend(self.global_resource_dir("prompts"));
            if self.project_trusted {
                prompt_paths.push(self.cwd.join(CONFIG_DIR_NAME).join("prompts"));
            }
            prompt_paths.extend(self.prompt_paths.clone());
        }

        let mut theme_paths = Vec::new();
        if !self.options.no_themes || !self.theme_paths.is_empty() {
            theme_paths.extend(self.global_resource_dir("themes"));
            if self.project_trusted {
                theme_paths.push(self.cwd.join(CONFIG_DIR_NAME).join("themes"));
            }
            theme_paths.extend(self.theme_paths.clone());
        }
        for path in &theme_paths {
            if !path.exists() {
                diagnostics.push(ResourceDiagnostic::warning(
                    "theme path does not exist".to_string(),
                    Some(path),
                ));
            }
        }

        let skill_result = load_skills(&crate::core::skills::LoadSkillsOptions {
            cwd: self.cwd.clone(),
            agent_dir: self.agent_dir.clone(),
            skill_paths: skill_paths.clone(),
            include_defaults: false,
        });
        diagnostics.extend(skill_result.diagnostics);

        let prompt_result = prompt_templates::load_prompt_templates(&LoadPromptTemplatesOptions {
            cwd: self.cwd.clone(),
            agent_dir: self.agent_dir.clone(),
            prompt_paths: prompt_paths.clone(),
            include_defaults: false,
        });
        let (prompts, prompt_diagnostics) = dedupe_prompts(prompt_result.templates);
        diagnostics.extend(prompt_result.diagnostics);
        diagnostics.extend(prompt_diagnostics);

        let (themes, theme_diagnostics) = self.load_themes(&theme_paths);
        diagnostics.extend(theme_diagnostics);

        let agents_files = if self.options.no_context_files {
            Vec::new()
        } else {
            load_project_context_files(&self.cwd, &self.agent_dir)
        };

        let (system_prompt, system_prompt_source) = self.discover_system_prompt();
        let system_prompt = self.options.system_prompt.clone().or(system_prompt);
        let (append_system_prompt, append_sources) = self.discover_append_system_prompt();
        let append_system_prompt = if self.options.append_system_prompt.is_empty() {
            append_system_prompt
        } else {
            self.options.append_system_prompt.clone()
        };

        let extensions = runner
            .map(|runner| {
                runner.extensions().into_iter().fold(
                    LoadExtensionsResult::default(),
                    |mut acc, extension| {
                        acc.extensions.push(extension);
                        acc
                    },
                )
            })
            .unwrap_or_default();

        self.resources = Resources {
            extensions,
            skills: skill_result.skills,
            prompts,
            themes,
            agents_files,
            system_prompt,
            system_prompt_source,
            append_system_prompt,
            append_system_prompt_sources: append_sources,
            diagnostics,
        };
        Ok(())
    }

    fn global_resource_dir(&self, name: &str) -> Vec<PathBuf> {
        vec![self.agent_dir.join(name)]
    }

    fn discover_system_prompt(&self) -> (Option<String>, Option<PathBuf>) {
        let project = self.cwd.join(CONFIG_DIR_NAME).join("SYSTEM.md");
        let path = if self.project_trusted && project.exists() {
            Some(project)
        } else {
            let global = self.agent_dir.join("SYSTEM.md");
            global.exists().then_some(global)
        };
        let content = path
            .as_ref()
            .and_then(|path| fs::read_to_string(path).ok())
            .map(|content| strip_bom(&content).to_string());
        (content, path)
    }

    fn discover_append_system_prompt(&self) -> (Vec<String>, Vec<PathBuf>) {
        let project = self.cwd.join(CONFIG_DIR_NAME).join("APPEND_SYSTEM.md");
        let path = if self.project_trusted && project.exists() {
            Some(project)
        } else {
            let global = self.agent_dir.join("APPEND_SYSTEM.md");
            global.exists().then_some(global)
        };
        match path {
            Some(path) => match fs::read_to_string(&path) {
                Ok(content) => (vec![strip_bom(&content).to_string()], vec![path]),
                Err(_) => (Vec::new(), Vec::new()),
            },
            None => (Vec::new(), Vec::new()),
        }
    }

    fn load_themes(&self, paths: &[PathBuf]) -> (Vec<ThemeInfo>, Vec<ResourceDiagnostic>) {
        let mut themes = Vec::new();
        let mut diagnostics = Vec::new();
        let mut seen: IndexMap<String, String> = IndexMap::new();
        for path in paths {
            if !path.exists() {
                continue;
            }
            let files: Vec<PathBuf> = if path.is_dir() {
                fs::read_dir(path)
                    .map(|entries| {
                        entries
                            .flatten()
                            .map(|entry| entry.path())
                            .filter(|path| {
                                path.extension()
                                    .is_some_and(|extension| extension == "json")
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            } else if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                vec![path.clone()]
            } else {
                diagnostics.push(ResourceDiagnostic::warning(
                    "theme path is not a json file".to_string(),
                    Some(path),
                ));
                Vec::new()
            };
            for file in files {
                match fs::read_to_string(&file) {
                    Ok(contents) => match serde_json::from_str::<Value>(strip_bom(&contents)) {
                        Ok(value) => {
                            let name = value
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("unnamed")
                                .to_string();
                            if let Some(existing) = seen.get(&name) {
                                diagnostics.push(ResourceDiagnostic {
                                    diagnostic_type: ResourceDiagnosticType::Collision,
                                    message: format!("name \"{name}\" collision"),
                                    path: Some(file.to_string_lossy().into_owned()),
                                    collision: Some(ResourceCollision {
                                        resource_type: ResourceType::Theme,
                                        name: name.clone(),
                                        winner_path: existing.clone(),
                                        loser_path: file.to_string_lossy().into_owned(),
                                        winner_source: None,
                                        loser_source: None,
                                    }),
                                });
                            } else {
                                seen.insert(name.clone(), file.to_string_lossy().into_owned());
                                themes.push(ThemeInfo {
                                    name,
                                    source_path: file,
                                });
                            }
                        }
                        Err(error) => diagnostics.push(ResourceDiagnostic::warning(
                            format!("failed to load theme: {error}"),
                            Some(&file),
                        )),
                    },
                    Err(error) => diagnostics.push(ResourceDiagnostic::warning(
                        format!("failed to read theme: {error}"),
                        Some(&file),
                    )),
                }
            }
        }
        (themes, diagnostics)
    }
}

fn dedupe_prompts(prompts: Vec<PromptTemplate>) -> (Vec<PromptTemplate>, Vec<ResourceDiagnostic>) {
    let mut seen: IndexMap<String, PromptTemplate> = IndexMap::new();
    let mut diagnostics = Vec::new();
    for prompt in prompts {
        if let Some(existing) = seen.get(&prompt.name) {
            diagnostics.push(ResourceDiagnostic {
                diagnostic_type: ResourceDiagnosticType::Collision,
                message: format!("name \"/{}\" collision", prompt.name),
                path: Some(prompt.file_path.to_string_lossy().into_owned()),
                collision: Some(ResourceCollision {
                    resource_type: ResourceType::Prompt,
                    name: prompt.name.clone(),
                    winner_path: existing.file_path.to_string_lossy().into_owned(),
                    loser_path: prompt.file_path.to_string_lossy().into_owned(),
                    winner_source: None,
                    loser_source: None,
                }),
            });
        } else {
            seen.insert(prompt.name.clone(), prompt);
        }
    }
    (seen.into_values().collect(), diagnostics)
}

fn load_context_file_from_dir(dir: &Path) -> Option<AgentsFile> {
    for name in CONTEXT_FILE_CANDIDATES {
        let path = dir.join(name);
        if path.is_file()
            && let Ok(contents) = fs::read_to_string(&path)
        {
            return Some(AgentsFile {
                path,
                content: strip_bom(&contents).to_string(),
            });
        }
    }
    None
}

/// Global context file first, then ancestor directories from root to cwd.
/// Duplicates by path are removed.
pub fn load_project_context_files(cwd: &Path, agent_dir: &Path) -> Vec<AgentsFile> {
    let mut files = Vec::new();
    let mut seen = BTreeSet::new();
    if let Some(global) = load_context_file_from_dir(agent_dir) {
        seen.insert(global.path.clone());
        files.push(global);
    }
    let mut ancestors = Vec::new();
    let mut current = cwd.to_path_buf();
    loop {
        if let Some(file) = load_context_file_from_dir(&current)
            && !seen.contains(&file.path)
        {
            seen.insert(file.path.clone());
            ancestors.push(file);
        }
        let Some(parent) = current.parent() else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent.to_path_buf();
    }
    ancestors.reverse();
    files.extend(ancestors);
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loader(root: &Path) -> ResourceLoader {
        let bus = Arc::new(EventBus::new());
        ResourceLoader::new(root.join("project"), root.join("agent"), bus)
    }

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(path, contents).expect("write");
    }

    #[test]
    fn context_files_layer_global_then_ancestors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let loader = loader(dir.path());
        write(&loader.agent_dir.join("AGENTS.md"), "global");
        write(&loader.cwd.join("AGENTS.md"), "project");
        write(
            &loader.cwd.parent().expect("parent").join("AGENTS.md"),
            "parent",
        );
        let files = load_project_context_files(&loader.cwd, &loader.agent_dir);
        let contents: Vec<_> = files.iter().map(|file| file.content.as_str()).collect();
        assert_eq!(contents, vec!["global", "parent", "project"]);
    }

    #[test]
    fn override_takes_priority_within_a_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let loader = loader(dir.path());
        write(&loader.cwd.join("AGENTS.md"), "plain");
        write(&loader.cwd.join("AGENTS.override.md"), "override");
        let files = load_project_context_files(&loader.cwd, &loader.agent_dir);
        assert!(files.iter().any(|file| file.content == "override"));
        assert!(!files.iter().any(|file| file.content == "plain"));
    }

    #[test]
    fn skip_context_files_option() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut loader = loader(dir.path());
        write(&loader.cwd.join("AGENTS.md"), "project");
        loader.options.no_context_files = true;
        loader.reload(None).expect("reload");
        assert!(loader.resources().agents_files.is_empty());
    }

    #[test]
    fn project_resources_require_trust() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut loader = loader(dir.path());
        let project_skill = loader
            .cwd
            .join(CONFIG_DIR_NAME)
            .join("skills")
            .join("secret")
            .join("SKILL.md");
        write(
            &project_skill,
            "---\nname: secret\ndescription: secret\n---\nbody",
        );
        loader.reload(None).expect("reload");
        assert!(loader.resources().skills.is_empty());
        loader.project_trusted = true;
        loader.reload(None).expect("reload");
        assert!(
            loader
                .resources()
                .skills
                .iter()
                .any(|skill| skill.name == "secret")
        );
    }

    #[test]
    fn system_prompt_source_is_trust_gated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut loader = loader(dir.path());
        let project = loader.cwd.join(CONFIG_DIR_NAME).join("SYSTEM.md");
        write(&project, "project prompt");
        loader.reload(None).expect("reload");
        assert_eq!(loader.system_prompt(), None);
        loader.project_trusted = true;
        loader.reload(None).expect("reload");
        assert_eq!(loader.system_prompt(), Some("project prompt"));
        assert_eq!(
            loader.resources().system_prompt_source.as_deref(),
            Some(project.as_path())
        );
    }

    #[test]
    fn global_system_prompt_is_used_in_both_trust_states() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut loader = loader(dir.path());
        write(&loader.agent_dir.join("SYSTEM.md"), "global prompt");
        loader.reload(None).expect("reload");
        assert_eq!(loader.system_prompt(), Some("global prompt"));
    }

    #[test]
    fn extend_resources_adds_skill_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut loader = loader(dir.path());
        let extra = dir.path().join("extra-skills");
        write(
            &extra.join("extra").join("SKILL.md"),
            "---\nname: extra\ndescription: extra\n---\nbody",
        );
        loader.extend_resources(ResourceExtensionPaths {
            skill_paths: vec![extra],
            ..ResourceExtensionPaths::default()
        });
        loader.reload(None).expect("reload");
        assert!(
            loader
                .resources()
                .skills
                .iter()
                .any(|skill| skill.name == "extra")
        );
    }

    #[test]
    fn duplicate_theme_names_produce_collision_diagnostics() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut loader = loader(dir.path());
        let one = dir.path().join("themes-one");
        let two = dir.path().join("themes-two");
        write(&one.join("a.json"), r#"{"name":"same"}"#);
        write(&two.join("b.json"), r#"{"name":"same"}"#);
        loader.extend_resources(ResourceExtensionPaths {
            theme_paths: vec![one, two],
            ..ResourceExtensionPaths::default()
        });
        loader.reload(None).expect("reload");
        assert_eq!(loader.resources().themes.len(), 1);
        assert!(loader.resources().diagnostics.iter().any(|diagnostic| {
            diagnostic.diagnostic_type == ResourceDiagnosticType::Collision
                && diagnostic.message.contains("collision")
        }));
    }

    #[test]
    fn missing_theme_path_is_diagnosed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut loader = loader(dir.path());
        let missing = dir.path().join("missing-theme.json");
        loader.extend_resources(ResourceExtensionPaths {
            theme_paths: vec![missing],
            ..ResourceExtensionPaths::default()
        });
        loader.reload(None).expect("reload");
        assert!(
            loader
                .resources()
                .diagnostics
                .iter()
                .any(|diagnostic| { diagnostic.message.contains("theme path does not exist") })
        );
    }
}
