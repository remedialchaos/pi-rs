//! `AGENTS.md`, skills, and prompt template discovery (`apps/p2/plugins/prompt/context-files.ts`).

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
};

use pi_rs_chord::Result;
use serde_json::Value;

use super::{
    contract::TemplateSource,
    templates::{PromptTemplate, SkillFile, parse_frontmatter, parse_template},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub kind: EntryKind,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoadedPromptSlice {
    pub agents: String,
    pub skills: String,
    pub skill_files: Vec<SkillFile>,
    pub templates: Vec<PromptTemplate>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoadedPrompt {
    pub agents: String,
    pub skills: String,
    pub fingerprint: String,
    pub skill_files: Vec<SkillFile>,
    pub templates: Vec<PromptTemplate>,
    pub project: LoadedPromptSlice,
}

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

pub trait PromptFiles: Send + Sync {
    fn is_local(&self) -> bool;
    fn read_text<'a>(&'a self, path: &'a str) -> BoxFut<'a, Option<String>>;
    fn entries<'a>(&'a self, path: &'a str) -> BoxFut<'a, Option<Vec<DirEntry>>>;
}

pub struct LocalPromptFiles;

impl PromptFiles for LocalPromptFiles {
    fn is_local(&self) -> bool {
        true
    }

    fn read_text<'a>(&'a self, path: &'a str) -> BoxFut<'a, Option<String>> {
        Box::pin(async move {
            match tokio::fs::read_to_string(path).await {
                Ok(text) => Ok(Some(text)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
    }

    fn entries<'a>(&'a self, path: &'a str) -> BoxFut<'a, Option<Vec<DirEntry>>> {
        Box::pin(async move {
            let mut dir = match tokio::fs::read_dir(path).await {
                Ok(d) => d,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e.into()),
            };
            let mut out = Vec::new();
            while let Some(entry) = dir.next_entry().await? {
                let ft = entry.file_type().await?;
                let kind = if ft.is_file() {
                    EntryKind::File
                } else if ft.is_dir() {
                    EntryKind::Directory
                } else {
                    EntryKind::Other
                };
                out.push(DirEntry {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    kind,
                });
            }
            Ok(Some(out))
        })
    }
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn join_path(base: &str, child: &str) -> String {
    Path::new(base).join(child).to_string_lossy().into_owned()
}

fn dirname_of(path: &str) -> String {
    Path::new(path)
        .parent()
        .map_or_else(String::new, |p| p.to_string_lossy().into_owned())
}

fn basename_of(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
}

/// Each `AGENTS.md` candidate from the filesystem root down to `cwd`.
pub fn agents_candidates(cwd: &str) -> Vec<String> {
    let mut ancestors = Vec::new();
    let mut cur = PathBuf::from(cwd);
    loop {
        ancestors.push(cur.join("AGENTS.md").to_string_lossy().into_owned());
        let Some(parent) = cur.parent() else {
            break;
        };
        if parent == cur {
            break;
        }
        cur = parent.to_path_buf();
    }
    ancestors.reverse();
    ancestors
}

fn skill_files<'a>(
    files: &'a dyn PromptFiles,
    path: &'a str,
    root: bool,
) -> Pin<Box<dyn Future<Output = Result<Vec<String>>> + Send + 'a>> {
    Box::pin(async move {
        let Some(mut entries) = files.entries(path).await? else {
            return Ok(Vec::new());
        };
        if entries
            .iter()
            .any(|e| e.kind == EntryKind::File && e.name == "SKILL.md")
        {
            return Ok(vec![join_path(path, "SKILL.md")]);
        }
        entries.retain(|e| !e.name.starts_with('.') && e.name != "node_modules");
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let mut found = Vec::new();
        for entry in entries {
            let child = join_path(path, &entry.name);
            match entry.kind {
                EntryKind::Directory => {
                    found.extend(skill_files(files, &child, false).await?);
                }
                EntryKind::File if root && entry.name.ends_with(".md") => {
                    found.push(child);
                }
                _ => {}
            }
        }
        Ok(found)
    })
}

async fn canonical(path: &str) -> String {
    tokio::fs::canonicalize(path)
        .await
        .map_or_else(|_| path.to_string(), |p| p.to_string_lossy().into_owned())
}

#[derive(Clone)]
struct DiscoveredSkill {
    name: String,
    description: String,
    path: String,
    disabled: bool,
    global: bool,
}

/// Loads `AGENTS.md`, skills, and prompt templates for a session.
pub async fn load_prompt_resources(
    directory: &str,
    cwd: &str,
    files: &dyn PromptFiles,
    skills_root: &str,
    prompts_root: &str,
    plugin_skill_roots: &[String],
) -> Result<LoadedPrompt> {
    let mut agents: Vec<(String, bool)> = Vec::new();
    let mut fingerprint: Vec<String> = Vec::new();

    let global_agents = join_path(directory, "AGENTS.md");
    let global_content = LocalPromptFiles.read_text(&global_agents).await?;
    fingerprint.push(format!(
        "{global_agents}\n{}",
        global_content.as_deref().unwrap_or("")
    ));
    if let Some(content) = global_content {
        let clean = content.strip_prefix('\u{FEFF}').unwrap_or(&content);
        agents.push((format!("## {global_agents}\n{clean}"), true));
    }

    let global_canon = if files.is_local() {
        Some(canonical(&global_agents).await)
    } else {
        None
    };

    for path in agents_candidates(cwd) {
        if let Some(gc) = &global_canon
            && canonical(&path).await == *gc
        {
            continue;
        }
        let content = files.read_text(&path).await?;
        fingerprint.push(format!("{path}\n{}", content.as_deref().unwrap_or("")));
        if let Some(content) = content {
            let clean = content.strip_prefix('\u{FEFF}').unwrap_or(&content);
            agents.push((format!("## {path}\n{clean}"), false));
        }
    }

    let env_skills = skill_files(files, skills_root, true).await?;
    let project_skills = skill_files(files, &join_path(cwd, ".pi/skills"), true).await?;
    let mut plugin_skills = Vec::new();
    for root in plugin_skill_roots {
        plugin_skills.extend(skill_files(files, root, true).await?);
    }

    let global_set: HashSet<String> = env_skills
        .iter()
        .chain(plugin_skills.iter())
        .cloned()
        .collect();
    let mut seen_paths = HashSet::new();
    let mut all_paths = Vec::new();
    for p in env_skills
        .into_iter()
        .chain(project_skills)
        .chain(plugin_skills)
    {
        if seen_paths.insert(p.clone()) {
            all_paths.push(p);
        }
    }

    let mut skills: HashMap<String, DiscoveredSkill> = HashMap::new();
    let mut skill_order: Vec<String> = Vec::new();
    for path in all_paths {
        let Some(content) = files.read_text(&path).await? else {
            continue;
        };
        fingerprint.push(format!("{path}\n{content}"));
        let (frontmatter, _) = parse_frontmatter(&content);
        let Some(desc) = frontmatter
            .get("description")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
        else {
            continue;
        };
        let name = frontmatter
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| basename_of(&dirname_of(&path)));
        if !skills.contains_key(&name) {
            skill_order.push(name.clone());
            skills.insert(
                name.clone(),
                DiscoveredSkill {
                    name,
                    description: desc.to_string(),
                    disabled: frontmatter
                        .get("disable-model-invocation")
                        .and_then(Value::as_bool)
                        == Some(true),
                    global: global_set.contains(&path),
                    path,
                },
            );
        }
    }

    let mut templates = Vec::new();
    let project_prompts = join_path(cwd, ".pi/prompts");
    for (root, source) in [
        (prompts_root, TemplateSource::Global),
        (project_prompts.as_str(), TemplateSource::Project),
    ] {
        let mut entries = files.entries(root).await?.unwrap_or_default();
        entries.retain(|e| e.kind == EntryKind::File && e.name.ends_with(".md"));
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        for entry in entries {
            let path = join_path(root, &entry.name);
            let Ok(Some(content)) = files.read_text(&path).await else {
                continue;
            };
            fingerprint.push(format!("{path}\n{content}"));
            let template = parse_template(&entry.name, &content, source, &path);
            if !template.name.is_empty()
                && !template.name.chars().any(|c| c.is_whitespace() || c == '/')
            {
                templates.push(template);
            }
        }
    }

    let assemble = |all: bool| -> LoadedPromptSlice {
        let kept: Vec<&DiscoveredSkill> = skill_order
            .iter()
            .filter_map(|k| skills.get(k))
            .filter(|s| all || !s.global)
            .collect();
        let visible: Vec<&DiscoveredSkill> = kept.iter().copied().filter(|s| !s.disabled).collect();
        let skills_xml = if visible.is_empty() {
            String::new()
        } else {
            let mut lines = vec![
                "Use the read tool to load a skill's file when the task matches its description."
                    .to_string(),
                "Resolve relative paths against the skill's directory.".to_string(),
                "<available_skills>".to_string(),
            ];
            for s in visible {
                lines.push(format!(
                    "<skill><name>{}</name><description>{}</description><location>{}</location></skill>",
                    escape_xml(&s.name),
                    escape_xml(&s.description),
                    escape_xml(&s.path)
                ));
            }
            lines.push("</available_skills>".to_string());
            lines.join("\n")
        };
        LoadedPromptSlice {
            agents: agents
                .iter()
                .filter(|(_, is_g)| all || !*is_g)
                .map(|(text, _)| text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n"),
            skills: skills_xml,
            skill_files: kept
                .into_iter()
                .map(|s| SkillFile {
                    name: s.name.clone(),
                    description: s.description.clone(),
                    path: s.path.clone(),
                    base_dir: dirname_of(&s.path),
                })
                .collect(),
            templates: templates
                .iter()
                .filter(|t| all || t.source != TemplateSource::Global)
                .cloned()
                .collect(),
        }
    };

    let full = assemble(true);
    let project = assemble(false);
    Ok(LoadedPrompt {
        agents: full.agents,
        skills: full.skills,
        fingerprint: fingerprint.join("\n\0\n"),
        skill_files: full.skill_files,
        templates: full.templates,
        project,
    })
}
