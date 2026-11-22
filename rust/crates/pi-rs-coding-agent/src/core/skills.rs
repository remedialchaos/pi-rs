//! Rust port of `packages/coding-agent/src/core/skills.ts`, including the local
//! helpers it relies on (`source-info`, `diagnostics`, `frontmatter`, `paths`).

use std::borrow::Cow;
use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use indexmap::IndexMap;
use saphyr::{LoadableYamlNode, Scalar, Yaml};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value};

const MAX_NAME_LENGTH: usize = 64;
const MAX_DESCRIPTION_LENGTH: usize = 1024;
const IGNORE_FILE_NAMES: [&str; 3] = [".gitignore", ".ignore", ".fdignore"];
pub const CONFIG_DIR_NAME: &str = ".pi";

// ---------------------------------------------------------------------------
// Local frontmatter and path helpers (crate::utils is owned by another unit).
// ---------------------------------------------------------------------------

pub fn strip_bom(content: &str) -> &str {
    content.strip_prefix('\u{feff}').unwrap_or(content)
}

fn normalize_newlines(content: &str) -> String {
    content.replace("\r\n", "\n").replace('\r', "\n")
}

fn yaml_error_text(yaml: &str, error: &saphyr::ScanError) -> String {
    let char_index = error.marker().index();
    let index = yaml
        .char_indices()
        .nth(char_index)
        .map_or(yaml.len(), |(byte, _)| byte);
    let line = yaml[..index].matches('\n').count() + 1;
    let line_start = yaml[..index].rfind('\n').map_or(0, |newline| newline + 1);
    let mut column = index - line_start + 1;
    // The `yaml` package points at the start of the offending plain scalar;
    // saphyr points at the second colon. Reproduce the upstream column.
    if error.info() == "mapping values are not allowed in this context"
        && let Some(offset) = yaml[line_start..index].rfind(": ")
    {
        column = offset + 3;
    }
    let line_text = yaml.lines().nth(line.saturating_sub(1)).unwrap_or("");
    format!(
        "{} at line {line}, column {column}:\n\n{line_text}\n{}^\n",
        error.info(),
        " ".repeat(column.saturating_sub(1))
    )
}

fn scalar_to_value(scalar: Scalar<'_>) -> Value {
    match scalar {
        Scalar::Null => Value::Null,
        Scalar::Boolean(value) => Value::Bool(value),
        Scalar::Integer(value) => Value::Number(value.into()),
        Scalar::FloatingPoint(value) => {
            serde_json::Number::from_f64(value.into_inner()).map_or(Value::Null, Value::Number)
        }
        Scalar::String(value) => Value::String(value.into_owned()),
    }
}

fn yaml_key_to_string(yaml: &Yaml<'_>) -> String {
    match yaml {
        Yaml::Value(scalar) => match scalar {
            Scalar::String(value) => value.to_string(),
            Scalar::Null => "null".to_string(),
            Scalar::Boolean(value) => value.to_string(),
            Scalar::Integer(value) => value.to_string(),
            Scalar::FloatingPoint(value) => value.to_string(),
        },
        Yaml::Representation(text, _, _) => text.to_string(),
        Yaml::Sequence(_)
        | Yaml::Mapping(_)
        | Yaml::Tagged(_, _)
        | Yaml::Alias(_)
        | Yaml::BadValue => String::new(),
    }
}

fn yaml_to_value(yaml: &Yaml<'_>) -> Value {
    match yaml {
        Yaml::Value(scalar) => scalar_to_value(scalar.clone()),
        Yaml::Representation(text, style, tag) => {
            Scalar::parse_from_cow_and_metadata(Cow::Owned(text.to_string()), *style, tag.as_ref())
                .map_or_else(|| Value::String(text.to_string()), scalar_to_value)
        }
        Yaml::Sequence(entries) => Value::Array(entries.iter().map(yaml_to_value).collect()),
        Yaml::Mapping(entries) => {
            let mut map = JsonMap::new();
            for (key, value) in entries {
                map.insert(yaml_key_to_string(key), yaml_to_value(value));
            }
            Value::Object(map)
        }
        Yaml::Tagged(_, inner) => yaml_to_value(inner),
        Yaml::Alias(_) | Yaml::BadValue => Value::Null,
    }
}

/// Parse YAML frontmatter into an insertion-ordered map and the trimmed body.
pub fn extract_frontmatter(content: &str) -> Result<(IndexMap<String, Value>, String), String> {
    let normalized = normalize_newlines(strip_bom(content));
    if !normalized.starts_with("---") {
        return Ok((IndexMap::new(), normalized));
    }
    let Some(end) = normalized[3..].find("\n---").map(|offset| offset + 3) else {
        return Ok((IndexMap::new(), normalized));
    };
    let yaml = normalized.get(4..end).unwrap_or("");
    let body = normalized.get(end + 4..).unwrap_or("").trim().to_string();
    if yaml.is_empty() {
        return Ok((IndexMap::new(), body));
    }

    let documents = Yaml::load_from_str(yaml).map_err(|error| yaml_error_text(yaml, &error))?;
    let Some(first) = documents.first() else {
        return Ok((IndexMap::new(), body));
    };
    match yaml_to_value(first) {
        Value::Object(map) => Ok((map.into_iter().collect(), body)),
        Value::Null => Ok((IndexMap::new(), body)),
        other => {
            let mut map = IndexMap::new();
            map.insert(String::new(), other);
            Ok((map, body))
        }
    }
}

pub fn home_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home);
    }
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        return PathBuf::from(profile);
    }
    PathBuf::from("/")
}

pub fn canonicalize_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    if result.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        result
    }
}

/// `resolvePath` semantics: expand `~`, make relative paths absolute, normalize lexically.
pub fn resolve_path(input: &Path, base: &Path, trim: bool) -> PathBuf {
    let mut text = input.to_string_lossy().into_owned();
    if trim {
        text = text.trim().to_string();
    }
    if text == "~" {
        return home_dir();
    }
    if let Some(rest) = text.strip_prefix("~/") {
        text = home_dir().join(rest).to_string_lossy().into_owned();
    }
    let path = PathBuf::from(text);
    if path.is_absolute() {
        lexical_normalize(&path)
    } else {
        lexical_normalize(&base.join(path))
    }
}

pub fn resolve_path_default(input: &Path) -> PathBuf {
    let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    resolve_path(input, &base, false)
}

pub fn path_comparison_key(path: &Path) -> String {
    let key = lexical_normalize(path).to_string_lossy().into_owned();
    if cfg!(windows) {
        key.to_lowercase()
    } else {
        key
    }
}

pub fn paths_equal(left: &Path, right: &Path) -> bool {
    path_comparison_key(left) == path_comparison_key(right)
}

pub fn is_path_within(parent: &Path, child: &Path) -> bool {
    let parent_key = path_comparison_key(parent);
    let child_key = path_comparison_key(child);
    if child_key == parent_key {
        return true;
    }
    let separator = if cfg!(windows) { '\\' } else { '/' };
    let prefix = if parent_key.ends_with(separator) {
        parent_key
    } else {
        format!("{parent_key}{separator}")
    };
    child_key.starts_with(&prefix)
}

pub fn to_posix_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Dedup key for resources: synthetic paths as-is, file paths canonicalized.
pub fn get_resource_path_key(path: &Path) -> String {
    let text = path.to_string_lossy();
    if text.starts_with("builtin:") || (text.starts_with('<') && text.ends_with('>')) {
        return text.into_owned();
    }
    path_comparison_key(&canonicalize_path(path))
}

/// Default agent dir (`~/.pi/agent`) unless `PI_CODING_AGENT_DIR` overrides it.
pub fn default_agent_dir() -> PathBuf {
    std::env::var_os("PI_CODING_AGENT_DIR")
        .map_or_else(|| home_dir().join(".pi").join("agent"), PathBuf::from)
}

// ---------------------------------------------------------------------------
// Source info and diagnostics
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceScope {
    User,
    Project,
    Temporary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceOrigin {
    Package,
    TopLevel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceInfo {
    pub path: String,
    pub source: String,
    pub scope: SourceScope,
    pub origin: SourceOrigin,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changelog_path: Option<String>,
}

pub fn create_synthetic_source_info(
    path: &Path,
    source: &str,
    scope: Option<SourceScope>,
    origin: Option<SourceOrigin>,
    base_dir: Option<&Path>,
) -> SourceInfo {
    SourceInfo {
        path: path.to_string_lossy().into_owned(),
        source: source.to_string(),
        scope: scope.unwrap_or(SourceScope::Temporary),
        origin: origin.unwrap_or(SourceOrigin::TopLevel),
        base_dir: base_dir.map(|path| path.to_string_lossy().into_owned()),
        changelog_path: None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceDiagnosticType {
    Warning,
    Error,
    Collision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceType {
    Extension,
    Skill,
    Prompt,
    Theme,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceCollision {
    pub resource_type: ResourceType,
    pub name: String,
    pub winner_path: String,
    pub loser_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub winner_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loser_source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceDiagnostic {
    #[serde(rename = "type")]
    pub diagnostic_type: ResourceDiagnosticType,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collision: Option<ResourceCollision>,
}

impl ResourceDiagnostic {
    pub fn warning(message: impl Into<String>, path: Option<&Path>) -> Self {
        Self {
            diagnostic_type: ResourceDiagnosticType::Warning,
            message: message.into(),
            path: path.map(|path| path.to_string_lossy().into_owned()),
            collision: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Skill model and validation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SkillFrontmatter {
    pub name: Option<String>,
    pub description: Option<Value>,
    #[serde(rename = "disable-model-invocation")]
    pub disable_model_invocation: Option<Value>,
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: PathBuf,
    pub base_dir: PathBuf,
    pub source_info: SourceInfo,
    pub disable_model_invocation: bool,
}

#[derive(Debug, Clone, Default)]
pub struct LoadSkillsResult {
    pub skills: Vec<Skill>,
    pub diagnostics: Vec<ResourceDiagnostic>,
}

#[derive(Debug, Clone)]
pub struct LoadSkillsFromDirOptions {
    pub dir: PathBuf,
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct LoadSkillsOptions {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub skill_paths: Vec<PathBuf>,
    pub include_defaults: bool,
}

pub fn validate_name(name: &str) -> Vec<String> {
    let mut errors = Vec::new();
    let name_length = name.encode_utf16().count();
    if name_length > MAX_NAME_LENGTH {
        errors.push(format!(
            "name exceeds {MAX_NAME_LENGTH} characters ({name_length})"
        ));
    }
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        errors.push(
            "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"
                .to_string(),
        );
    }
    if name.starts_with('-') || name.ends_with('-') {
        errors.push("name must not start or end with a hyphen".to_string());
    }
    if name.contains("--") {
        errors.push("name must not contain consecutive hyphens".to_string());
    }
    errors
}

pub fn validate_description(description: Option<&Value>) -> Vec<String> {
    let mut errors = Vec::new();
    match description {
        Some(Value::String(text)) if !text.trim().is_empty() => {
            let description_length = text.encode_utf16().count();
            if description_length > MAX_DESCRIPTION_LENGTH {
                errors.push(format!(
                    "description exceeds {MAX_DESCRIPTION_LENGTH} characters ({description_length})"
                ));
            }
        }
        _ => errors.push("description is required".to_string()),
    }
    errors
}

fn create_skill_source_info(file_path: &Path, base_dir: &Path, source: &str) -> SourceInfo {
    match source {
        "user" => create_synthetic_source_info(
            file_path,
            "local",
            Some(SourceScope::User),
            None,
            Some(base_dir),
        ),
        "project" => create_synthetic_source_info(
            file_path,
            "local",
            Some(SourceScope::Project),
            None,
            Some(base_dir),
        ),
        "path" => create_synthetic_source_info(file_path, "local", None, None, Some(base_dir)),
        other => create_synthetic_source_info(file_path, other, None, None, Some(base_dir)),
    }
}

// ---------------------------------------------------------------------------
// Ignore handling
// ---------------------------------------------------------------------------

type IgnoreMatcher = Vec<Gitignore>;

fn prefix_ignore_pattern(line: &str, prefix: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('#') && !trimmed.starts_with("\\#") {
        return None;
    }

    let mut pattern = line.to_string();
    let mut negated = false;
    if let Some(rest) = pattern.strip_prefix('!') {
        negated = true;
        pattern = rest.to_string();
    } else if let Some(rest) = pattern.strip_prefix("\\!") {
        pattern = rest.to_string();
    }
    if let Some(rest) = pattern.strip_prefix('/') {
        pattern = rest.to_string();
    }

    let prefixed = format!("{prefix}{pattern}");
    Some(if negated {
        format!("!{prefixed}")
    } else {
        prefixed
    })
}

fn add_ignore_rules(matcher: &mut IgnoreMatcher, dir: &Path, root: &Path) {
    let relative = dir.strip_prefix(root).unwrap_or(Path::new(""));
    let relative_text = to_posix_path(relative);
    let prefix = if relative_text.is_empty() {
        String::new()
    } else {
        format!("{relative_text}/")
    };

    for filename in IGNORE_FILE_NAMES {
        let ignore_path = dir.join(filename);
        let Ok(content) = fs::read_to_string(&ignore_path) else {
            continue;
        };
        let patterns: Vec<String> = content
            .split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
            .filter_map(|line| prefix_ignore_pattern(line, &prefix))
            .collect();
        if patterns.is_empty() {
            continue;
        }
        let mut builder = GitignoreBuilder::new("");
        for pattern in patterns {
            let _unused = builder.add_line(None, &pattern);
        }
        if let Ok(gitignore) = builder.build() {
            matcher.push(gitignore);
        }
    }
}

fn is_ignored(matcher: &IgnoreMatcher, relative_path: &str, is_dir: bool) -> bool {
    let path = Path::new(relative_path);
    let mut ignored = false;
    for gitignore in matcher {
        match gitignore.matched(path, is_dir) {
            ignore::Match::Ignore(_) => ignored = true,
            ignore::Match::Whitelist(_) => ignored = false,
            ignore::Match::None => {}
        }
    }
    ignored
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

fn load_skill_from_file(
    file_path: &Path,
    source: &str,
) -> (Option<Skill>, Vec<ResourceDiagnostic>) {
    let mut diagnostics = Vec::new();
    let is_declared_skill = file_path.file_name().is_some_and(|name| name == "SKILL.md");

    let raw_content = match fs::read_to_string(file_path) {
        Ok(content) => content,
        Err(error) => {
            diagnostics.push(ResourceDiagnostic::warning(
                format!("{error}"),
                Some(file_path),
            ));
            return (None, diagnostics);
        }
    };

    let (frontmatter_map, _body) = match extract_frontmatter(&raw_content) {
        Ok(value) => value,
        Err(message) => {
            if is_declared_skill {
                diagnostics.push(ResourceDiagnostic::warning(message, Some(file_path)));
            }
            return (None, diagnostics);
        }
    };
    let frontmatter: SkillFrontmatter =
        serde_json::from_value(Value::Object(frontmatter_map.into_iter().collect()))
            .unwrap_or_default();

    let description_value = frontmatter.description.as_ref();
    let has_description = description_value
        .and_then(Value::as_str)
        .is_some_and(|text| !text.trim().is_empty());
    if !is_declared_skill && !has_description {
        return (None, diagnostics);
    }

    for message in validate_description(description_value) {
        diagnostics.push(ResourceDiagnostic::warning(message, Some(file_path)));
    }

    let parent_dir_name = file_path
        .parent()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = frontmatter
        .name
        .as_deref()
        .filter(|name| !name.is_empty())
        .unwrap_or(&parent_dir_name)
        .to_string();

    for message in validate_name(&name) {
        diagnostics.push(ResourceDiagnostic::warning(message, Some(file_path)));
    }

    if !has_description {
        return (None, diagnostics);
    }
    let description = description_value
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let base_dir = file_path
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);

    let skill = Skill {
        name,
        description,
        file_path: file_path.to_path_buf(),
        base_dir: base_dir.clone(),
        source_info: create_skill_source_info(file_path, &base_dir, source),
        disable_model_invocation: frontmatter.disable_model_invocation == Some(Value::Bool(true)),
    };
    (Some(skill), diagnostics)
}

fn load_skills_from_dir_internal(
    dir: &Path,
    source: &str,
    include_root_files: bool,
    matcher: &mut IgnoreMatcher,
    root: &Path,
) -> LoadSkillsResult {
    let mut result = LoadSkillsResult::default();
    if !dir.exists() {
        return result;
    }

    add_ignore_rules(matcher, dir, root);

    let Ok(read_dir) = fs::read_dir(dir) else {
        return result;
    };
    let entries: Vec<fs::DirEntry> = read_dir.filter_map(|entry| entry.ok()).collect();

    // Pass 1: a directory containing SKILL.md is a skill root; stop recursing.
    for entry in &entries {
        if entry.file_name() != "SKILL.md" {
            continue;
        }
        let full_path = entry.path();
        let is_file = fs::metadata(&full_path).is_ok_and(|metadata| metadata.is_file());
        let relative = full_path.strip_prefix(root).unwrap_or(Path::new(""));
        let relative_text = to_posix_path(relative);
        if !is_file || is_ignored(matcher, &relative_text, false) {
            continue;
        }
        let (skill, diagnostics) = load_skill_from_file(&full_path, source);
        if let Some(skill) = skill {
            result.skills.push(skill);
        }
        result.diagnostics.extend(diagnostics);
        return result;
    }

    // Pass 2: recurse into subdirectories; only scan roots load plain .md files.
    for entry in &entries {
        let name = entry.file_name();
        let name_text = name.to_string_lossy();
        if name_text.starts_with('.') || name_text == "node_modules" {
            continue;
        }
        let full_path = entry.path();
        let Ok(metadata) = fs::metadata(&full_path) else {
            continue;
        };
        let is_dir = metadata.is_dir();
        let is_file = metadata.is_file();
        let relative = full_path.strip_prefix(root).unwrap_or(Path::new(""));
        let relative_text = to_posix_path(relative);
        if is_ignored(matcher, &relative_text, is_dir) {
            continue;
        }
        if is_dir {
            let sub = load_skills_from_dir_internal(&full_path, source, false, matcher, root);
            result.skills.extend(sub.skills);
            result.diagnostics.extend(sub.diagnostics);
            continue;
        }
        if is_file && include_root_files && name_text.ends_with(".md") {
            let (skill, diagnostics) = load_skill_from_file(&full_path, source);
            if let Some(skill) = skill {
                result.skills.push(skill);
            }
            result.diagnostics.extend(diagnostics);
        }
    }
    result
}

/// Load skills from a single directory.
pub fn load_skills_from_dir(options: &LoadSkillsFromDirOptions) -> LoadSkillsResult {
    let mut matcher = Vec::new();
    load_skills_from_dir_internal(
        &options.dir,
        &options.source,
        true,
        &mut matcher,
        &options.dir,
    )
}

/// How a skill's file is read at agent runtime.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FileReadTool {
    #[default]
    Read,
    Bash,
    Indirect,
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Format visible skills for the system prompt.
pub fn format_skills_for_prompt(skills: &[Skill], file_read_tool: FileReadTool) -> String {
    let visible: Vec<&Skill> = skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation)
        .collect();
    if visible.is_empty() {
        return String::new();
    }

    let mut lines = vec![
        "\n\nThe following skills provide specialized instructions for specific tasks.".to_string(),
        match file_read_tool {
            FileReadTool::Read => "Use the read tool to load a skill's file when the task matches its description.".to_string(),
            FileReadTool::Bash => "Use bash to load a skill's file when the task matches its description.".to_string(),
            FileReadTool::Indirect => "Load a skill's file when the task matches its description.".to_string(),
        },
        "Resolve relative paths against the skill's directory (parent of SKILL.md). Use absolute paths in tool calls.".to_string(),
        String::new(),
        "<available_skills>".to_string(),
    ];
    for skill in visible {
        lines.push("  <skill>".to_string());
        lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!(
            "    <description>{}</description>",
            escape_xml(&skill.description)
        ));
        lines.push(format!(
            "    <location>{}</location>",
            escape_xml(&skill.file_path.to_string_lossy())
        ));
        lines.push("  </skill>".to_string());
    }
    lines.push("</available_skills>".to_string());
    lines.join("\n")
}

fn add_skills(
    result: &LoadSkillsResult,
    skill_map: &mut IndexMap<String, Skill>,
    real_paths: &mut HashSet<String>,
    diagnostics: &mut Vec<ResourceDiagnostic>,
    collisions: &mut Vec<ResourceDiagnostic>,
) {
    diagnostics.extend(result.diagnostics.iter().cloned());
    for skill in &result.skills {
        let real_path = get_resource_path_key(&skill.file_path);
        if real_paths.contains(&real_path) {
            continue;
        }
        if let Some(existing) = skill_map.get(&skill.name) {
            collisions.push(ResourceDiagnostic {
                diagnostic_type: ResourceDiagnosticType::Collision,
                message: format!("name \"{}\" collision", skill.name),
                path: Some(skill.file_path.to_string_lossy().into_owned()),
                collision: Some(ResourceCollision {
                    resource_type: ResourceType::Skill,
                    name: skill.name.clone(),
                    winner_path: existing.file_path.to_string_lossy().into_owned(),
                    loser_path: skill.file_path.to_string_lossy().into_owned(),
                    winner_source: None,
                    loser_source: None,
                }),
            });
        } else {
            skill_map.insert(skill.name.clone(), skill.clone());
            real_paths.insert(real_path);
        }
    }
}

/// Load skills from default directories (user, project) and explicit paths.
pub fn load_skills(options: &LoadSkillsOptions) -> LoadSkillsResult {
    let resolved_cwd = resolve_path_default(&options.cwd);
    let resolved_agent_dir = resolve_path_default(&options.agent_dir);

    let mut skill_map = IndexMap::new();
    let mut real_paths = HashSet::new();
    let mut diagnostics = Vec::new();
    let mut collisions = Vec::new();

    let user_skills_dir = resolved_agent_dir.join("skills");
    let project_skills_dir = resolved_cwd.join(CONFIG_DIR_NAME).join("skills");

    if options.include_defaults {
        let mut matcher = Vec::new();
        let user = load_skills_from_dir_internal(
            &user_skills_dir,
            "user",
            true,
            &mut matcher,
            &user_skills_dir,
        );
        add_skills(
            &user,
            &mut skill_map,
            &mut real_paths,
            &mut diagnostics,
            &mut collisions,
        );
        let mut matcher = Vec::new();
        let project = load_skills_from_dir_internal(
            &project_skills_dir,
            "project",
            true,
            &mut matcher,
            &project_skills_dir,
        );
        add_skills(
            &project,
            &mut skill_map,
            &mut real_paths,
            &mut diagnostics,
            &mut collisions,
        );
    }

    let get_source = |path: &Path| -> &'static str {
        if !options.include_defaults {
            if is_path_within(&user_skills_dir, path) {
                return "user";
            }
            if is_path_within(&project_skills_dir, path) {
                return "project";
            }
        }
        "path"
    };

    for raw_path in &options.skill_paths {
        let resolved_path = resolve_path(raw_path, &resolved_cwd, true);
        if !resolved_path.exists() {
            diagnostics.push(ResourceDiagnostic::warning(
                "skill path does not exist",
                Some(&resolved_path),
            ));
            continue;
        }
        let metadata = match fs::metadata(&resolved_path) {
            Ok(metadata) => metadata,
            Err(error) => {
                diagnostics.push(ResourceDiagnostic::warning(
                    format!("{error}"),
                    Some(&resolved_path),
                ));
                continue;
            }
        };
        let source = get_source(&resolved_path);
        if metadata.is_dir() {
            let mut matcher = Vec::new();
            let loaded = load_skills_from_dir_internal(
                &resolved_path,
                source,
                true,
                &mut matcher,
                &resolved_path,
            );
            add_skills(
                &loaded,
                &mut skill_map,
                &mut real_paths,
                &mut diagnostics,
                &mut collisions,
            );
        } else if metadata.is_file() && resolved_path.to_string_lossy().ends_with(".md") {
            let (skill, file_diagnostics) = load_skill_from_file(&resolved_path, source);
            if let Some(skill) = skill {
                add_skills(
                    &LoadSkillsResult {
                        skills: vec![skill],
                        diagnostics: file_diagnostics,
                    },
                    &mut skill_map,
                    &mut real_paths,
                    &mut diagnostics,
                    &mut collisions,
                );
            } else {
                diagnostics.extend(file_diagnostics);
            }
        } else {
            diagnostics.push(ResourceDiagnostic::warning(
                "skill path is not a markdown file",
                Some(&resolved_path),
            ));
        }
    }

    diagnostics.extend(collisions);
    LoadSkillsResult {
        skills: skill_map.into_values().collect(),
        diagnostics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| format!("create dir: {error}"))?;
        }
        fs::write(path, content).map_err(|error| format!("write: {error}"))
    }

    fn skill_file(dir: &Path, name: &str, content: &str) -> Result<PathBuf, String> {
        let path = dir.join(name).join("SKILL.md");
        write(&path, content)?;
        Ok(path)
    }

    const VALID: &str =
        "---\nname: valid-skill\ndescription: A valid skill for testing purposes.\n---\nBody\n";

    #[test]
    fn valid_skill_fixture() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| format!("tempdir: {error}"))?;
        skill_file(dir.path(), "valid-skill", VALID)?;
        let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
            dir: dir.path().to_path_buf(),
            source: "test".to_string(),
        });
        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.skills[0].name, "valid-skill");
        assert_eq!(
            result.skills[0].description,
            "A valid skill for testing purposes."
        );
        assert_eq!(result.skills[0].source_info.source, "test");
        assert!(result.diagnostics.is_empty());
        Ok(())
    }

    #[test]
    fn frontmatter_name_overrides_directory() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| format!("tempdir: {error}"))?;
        skill_file(dir.path(), "some-dir", VALID)?;
        let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
            dir: dir.path().to_path_buf(),
            source: "test".to_string(),
        });
        assert_eq!(result.skills[0].name, "valid-skill");
        assert!(result.diagnostics.is_empty());
        Ok(())
    }

    #[test]
    fn invalid_name_chars_warn_but_skill_loads() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| format!("tempdir: {error}"))?;
        skill_file(
            dir.path(),
            "bad",
            "---\nname: Invalid_Name\ndescription: Desc.\n---\n",
        )?;
        let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
            dir: dir.path().to_path_buf(),
            source: "test".to_string(),
        });
        assert_eq!(result.skills.len(), 1);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("invalid characters"))
        );
        Ok(())
    }

    #[test]
    fn long_name_and_consecutive_hyphens() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| format!("tempdir: {error}"))?;
        skill_file(
            dir.path(),
            "long",
            &format!("---\nname: {}\ndescription: Desc.\n---\n", "a".repeat(65)),
        )?;
        skill_file(
            dir.path(),
            "hyphen",
            "---\nname: bad--name\ndescription: Desc.\n---\n",
        )?;
        let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
            dir: dir.path().to_path_buf(),
            source: "test".to_string(),
        });
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("exceeds 64 characters"))
        );
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("consecutive hyphens"))
        );
        Ok(())
    }

    #[test]
    fn missing_description_drops_skill() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| format!("tempdir: {error}"))?;
        skill_file(dir.path(), "missing", "---\nname: missing\n---\n")?;
        let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
            dir: dir.path().to_path_buf(),
            source: "test".to_string(),
        });
        assert!(result.skills.is_empty());
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("description is required"))
        );
        Ok(())
    }

    #[test]
    fn unknown_frontmatter_fields_are_ignored() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| format!("tempdir: {error}"))?;
        skill_file(
            dir.path(),
            "extra",
            "---\nname: extra\ndescription: Desc.\nauthor: someone\nversion: 1\n---\n",
        )?;
        let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
            dir: dir.path().to_path_buf(),
            source: "test".to_string(),
        });
        assert_eq!(result.skills.len(), 1);
        assert!(result.diagnostics.is_empty());
        Ok(())
    }

    #[test]
    fn root_skill_md_short_circuits_nested_skills() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| format!("tempdir: {error}"))?;
        skill_file(
            dir.path(),
            "root",
            "---\nname: root-skill\ndescription: Root skill should win.\n---\n",
        )?;
        skill_file(
            &dir.path().join("root"),
            "child",
            "---\nname: child-skill\ndescription: Child.\n---\n",
        )?;
        let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
            dir: dir.path().to_path_buf(),
            source: "test".to_string(),
        });
        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.skills[0].name, "root-skill");
        Ok(())
    }

    #[test]
    fn nested_skill_discovered() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| format!("tempdir: {error}"))?;
        skill_file(
            &dir.path().join("parent").join("child"),
            "ignored",
            "---\nname: child-skill\ndescription: Nested.\n---\n",
        )?;
        let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
            dir: dir.path().to_path_buf(),
            source: "test".to_string(),
        });
        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.skills[0].name, "child-skill");
        Ok(())
    }

    #[test]
    fn invalid_yaml_reports_line() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| format!("tempdir: {error}"))?;
        skill_file(dir.path(), "bad", "---\n[unclosed bracket\n---\n")?;
        let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
            dir: dir.path().to_path_buf(),
            source: "test".to_string(),
        });
        assert!(result.skills.is_empty());
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("at line"))
        );
        Ok(())
    }

    #[test]
    fn multiline_description_is_preserved() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| format!("tempdir: {error}"))?;
        skill_file(
            dir.path(),
            "multi",
            "---\nname: multi\ndescription: |\n  This is a multiline description.\n  Second line.\n---\n",
        )?;
        let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
            dir: dir.path().to_path_buf(),
            source: "test".to_string(),
        });
        assert_eq!(result.skills.len(), 1);
        assert!(result.skills[0].description.contains('\n'));
        assert!(
            result.skills[0]
                .description
                .contains("This is a multiline description.")
        );
        Ok(())
    }

    #[test]
    fn disable_model_invocation_requires_boolean_true() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| format!("tempdir: {error}"))?;
        skill_file(
            dir.path(),
            "hidden",
            "---\nname: hidden\ndescription: Hidden.\ndisable-model-invocation: true\n---\n",
        )?;
        skill_file(
            dir.path(),
            "shown",
            "---\nname: shown\ndescription: Shown.\ndisable-model-invocation: \"true\"\n---\n",
        )?;
        let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
            dir: dir.path().to_path_buf(),
            source: "test".to_string(),
        });
        let hidden = result
            .skills
            .iter()
            .find(|skill| skill.name == "hidden")
            .ok_or("no hidden skill")?;
        let shown = result
            .skills
            .iter()
            .find(|skill| skill.name == "shown")
            .ok_or("no shown skill")?;
        assert!(hidden.disable_model_invocation);
        assert!(!shown.disable_model_invocation);
        Ok(())
    }

    #[test]
    fn ignore_files_are_honoured() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| format!("tempdir: {error}"))?;
        write(&dir.path().join(".gitignore"), "ignored/\n")?;
        skill_file(
            dir.path(),
            "shown",
            "---\nname: shown\ndescription: Shown.\n---\n",
        )?;
        skill_file(
            &dir.path().join("ignored"),
            "hidden",
            "---\nname: hidden\ndescription: Hidden.\n---\n",
        )?;
        let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
            dir: dir.path().to_path_buf(),
            source: "test".to_string(),
        });
        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.skills[0].name, "shown");
        Ok(())
    }

    #[test]
    fn missing_dir_is_empty() {
        let result = load_skills_from_dir(&LoadSkillsFromDirOptions {
            dir: PathBuf::from("/definitely/not/here"),
            source: "test".to_string(),
        });
        assert!(result.skills.is_empty());
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn formatting_empty_and_xml_escaping() -> Result<(), String> {
        assert_eq!(format_skills_for_prompt(&[], FileReadTool::Read), "");
        let skill = Skill {
            name: "test-skill".to_string(),
            description: "A <special> & \"characters\"".to_string(),
            file_path: PathBuf::from("/path/to/skill/SKILL.md"),
            base_dir: PathBuf::from("/path/to/skill"),
            source_info: create_synthetic_source_info(
                Path::new("/path/to/skill/SKILL.md"),
                "test",
                None,
                None,
                None,
            ),
            disable_model_invocation: false,
        };
        let formatted = format_skills_for_prompt(&[skill], FileReadTool::Read);
        assert!(formatted.contains("<available_skills>"));
        assert!(formatted.contains("<name>test-skill</name>"));
        assert!(formatted.contains("&lt;special&gt;"));
        assert!(formatted.contains("&amp;"));
        assert!(formatted.contains("&quot;characters&quot;"));
        assert!(formatted.contains("<location>/path/to/skill/SKILL.md</location>"));
        Ok(())
    }

    #[test]
    fn hidden_skills_excluded_from_prompt() {
        let visible = Skill {
            name: "visible".to_string(),
            description: "Visible.".to_string(),
            file_path: PathBuf::from("/tmp/visible/SKILL.md"),
            base_dir: PathBuf::from("/tmp/visible"),
            source_info: create_synthetic_source_info(
                Path::new("/tmp/visible/SKILL.md"),
                "test",
                None,
                None,
                None,
            ),
            disable_model_invocation: false,
        };
        let mut hidden = visible.clone();
        hidden.name = "hidden".to_string();
        hidden.disable_model_invocation = true;
        let formatted = format_skills_for_prompt(&[visible.clone(), hidden], FileReadTool::Read);
        assert!(!formatted.contains("hidden"));
        assert_eq!(
            formatted,
            format_skills_for_prompt(&[visible], FileReadTool::Read)
        );
    }

    #[test]
    fn explicit_paths_have_temporary_scope_and_missing_paths_warn() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| format!("tempdir: {error}"))?;
        let skill_dir = dir.path().join("explicit");
        skill_file(&skill_dir, "unused", VALID)?;
        let options = LoadSkillsOptions {
            cwd: dir.path().to_path_buf(),
            agent_dir: dir.path().join("agent"),
            skill_paths: vec![skill_dir, dir.path().join("missing")],
            include_defaults: false,
        };
        let result = load_skills(&options);
        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.skills[0].source_info.scope, SourceScope::Temporary);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message == "skill path does not exist")
        );
        Ok(())
    }

    #[test]
    fn first_skill_wins_name_collision() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| format!("tempdir: {error}"))?;
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        skill_file(
            &first,
            "calendar",
            "---\nname: calendar\ndescription: First.\n---\n",
        )?;
        skill_file(
            &second,
            "calendar",
            "---\nname: calendar\ndescription: Second.\n---\n",
        )?;
        let result = load_skills(&LoadSkillsOptions {
            cwd: dir.path().to_path_buf(),
            agent_dir: dir.path().join("agent"),
            skill_paths: vec![first.clone(), second.clone()],
            include_defaults: false,
        });
        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.skills[0].description, "First.");
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].message, "name \"calendar\" collision");
        Ok(())
    }

    #[test]
    fn duplicate_file_is_deduplicated_silently() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| format!("tempdir: {error}"))?;
        let path = skill_file(dir.path(), "one", VALID)?;
        let result = load_skills(&LoadSkillsOptions {
            cwd: dir.path().to_path_buf(),
            agent_dir: dir.path().join("agent"),
            skill_paths: vec![path.clone(), path],
            include_defaults: false,
        });
        assert_eq!(result.skills.len(), 1);
        assert!(result.diagnostics.is_empty());
        Ok(())
    }

    #[test]
    fn tilde_agent_dir_matches_absolute_home() {
        let home = home_dir();
        assert_eq!(
            resolve_path_default(Path::new("~/.pi/agent")),
            home.join(".pi").join("agent")
        );
    }
}
