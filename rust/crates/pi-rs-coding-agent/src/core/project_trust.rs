//! Rust port of `packages/coding-agent/src/core/trust-manager.ts` and
//! `packages/coding-agent/src/core/project-trust.ts`.

use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::lockfile::{LockGuard, LockOptions, lock_sync};

use super::skills::{
    CONFIG_DIR_NAME, canonicalize_path, home_dir, paths_equal, resolve_path_default, strip_bom,
};

const APP_NAME: &str = "pi";
const TRUST_REQUIRING_PROJECT_CONFIG_RESOURCES: [&str; 8] = [
    "settings.json",
    "mcp.json",
    "extensions",
    "skills",
    "prompts",
    "themes",
    "SYSTEM.md",
    "APPEND_SYSTEM.md",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppMode {
    Interactive,
    Print,
    Json,
    Rpc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DefaultProjectTrust {
    Always,
    Never,
    Ask,
}

pub type ProjectTrustDecision = Option<bool>;

#[derive(Debug, Clone)]
pub struct ProjectTrustStoreEntry {
    pub path: String,
    pub decision: bool,
}

#[derive(Debug, Clone)]
pub struct ProjectTrustUpdate {
    pub path: String,
    pub decision: ProjectTrustDecision,
}

#[derive(Debug, Clone)]
pub struct ProjectTrustOption {
    pub label: String,
    pub trusted: bool,
    pub updates: Vec<ProjectTrustUpdate>,
    pub saved_path: Option<String>,
}

#[derive(Debug)]
pub enum TrustError {
    Io(String),
    Lock(String),
    Format(String),
}

impl fmt::Display for TrustError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message) | Self::Lock(message) | Self::Format(message) => {
                formatter.write_str(message)
            }
        }
    }
}

impl Error for TrustError {}

type TrustFile = IndexMap<String, Option<bool>>;

pub fn normalize_cwd(cwd: &Path) -> PathBuf {
    canonicalize_path(&resolve_path_default(cwd))
}

fn find_trust_file_keys(data: &TrustFile, path: &Path) -> Vec<String> {
    data.keys()
        .filter(|key| paths_equal(Path::new(key.as_str()), path))
        .cloned()
        .collect()
}

pub fn find_nearest_trust_entry(data: &TrustFile, cwd: &Path) -> Option<ProjectTrustStoreEntry> {
    let mut current = normalize_cwd(cwd);
    loop {
        for key in find_trust_file_keys(data, &current) {
            if let Some(decision) = data.get(&key).copied().flatten() {
                return Some(ProjectTrustStoreEntry {
                    path: key,
                    decision,
                });
            }
        }
        let parent = current.parent()?.to_path_buf();
        if parent == current {
            return None;
        }
        current = parent;
    }
}

pub fn get_project_trust_parent_path(cwd: &Path) -> Option<PathBuf> {
    let trust_path = normalize_cwd(cwd);
    let parent = trust_path.parent()?.to_path_buf();
    (parent != trust_path).then_some(parent)
}

pub fn get_project_trust_options(
    cwd: &Path,
    include_session_only: bool,
) -> Vec<ProjectTrustOption> {
    let trust_path = normalize_cwd(cwd);
    let trust_path_text = trust_path.to_string_lossy().into_owned();
    let mut options = vec![ProjectTrustOption {
        label: "Trust".to_string(),
        trusted: true,
        updates: vec![ProjectTrustUpdate {
            path: trust_path_text.clone(),
            decision: Some(true),
        }],
        saved_path: Some(trust_path_text.clone()),
    }];

    if let Some(parent) = get_project_trust_parent_path(cwd) {
        let parent_text = parent.to_string_lossy().into_owned();
        options.push(ProjectTrustOption {
            label: format!("Trust parent folder ({parent_text})"),
            trusted: true,
            updates: vec![
                ProjectTrustUpdate {
                    path: parent_text.clone(),
                    decision: Some(true),
                },
                ProjectTrustUpdate {
                    path: trust_path_text.clone(),
                    decision: None,
                },
            ],
            saved_path: Some(parent_text),
        });
    }

    if include_session_only {
        options.push(ProjectTrustOption {
            label: "Trust (this session only)".to_string(),
            trusted: true,
            updates: Vec::new(),
            saved_path: None,
        });
    }
    options.push(ProjectTrustOption {
        label: "Do not trust".to_string(),
        trusted: false,
        updates: vec![ProjectTrustUpdate {
            path: trust_path_text.clone(),
            decision: Some(false),
        }],
        saved_path: Some(trust_path_text),
    });
    if include_session_only {
        options.push(ProjectTrustOption {
            label: "Do not trust (this session only)".to_string(),
            trusted: false,
            updates: Vec::new(),
            saved_path: None,
        });
    }
    options
}

pub fn read_trust_file(path: &Path) -> Result<TrustFile, TrustError> {
    if !path.exists() {
        return Ok(TrustFile::new());
    }
    let content = fs::read_to_string(path).map_err(|error| TrustError::Io(format!("{error}")))?;
    let parsed: Value = serde_json::from_str(strip_bom(&content)).map_err(|error| {
        TrustError::Format(format!(
            "Failed to read trust store {}: {error}",
            path.display()
        ))
    })?;
    let Value::Object(object) = parsed else {
        return Err(TrustError::Format(format!(
            "Invalid trust store {}: expected an object",
            path.display()
        )));
    };

    let mut data = TrustFile::new();
    for (key, value) in object {
        let decision = match value {
            Value::Bool(value) => Some(value),
            Value::Null => None,
            _ => {
                let quoted_key = serde_json::to_string(&key)
                    .map_err(|error| TrustError::Format(format!("{error}")))?;
                return Err(TrustError::Format(format!(
                    "Invalid trust store {}: value for {quoted_key} must be true, false, or null",
                    path.display()
                )));
            }
        };
        data.insert(key, decision);
    }
    Ok(data)
}

pub fn write_trust_file(path: &Path, data: &TrustFile) -> Result<(), TrustError> {
    let mut keys: Vec<&String> = data.keys().collect();
    keys.sort();
    let mut sorted = serde_json::Map::new();
    for key in keys {
        if let Some(decision) = data.get(key).copied().flatten() {
            sorted.insert(key.clone(), Value::Bool(decision));
        } else if data.get(key).copied().unwrap_or(None).is_none() && data.contains_key(key) {
            sorted.insert(key.clone(), Value::Null);
        }
    }
    let content = serde_json::to_string_pretty(&Value::Object(sorted))
        .map_err(|error| TrustError::Format(format!("{error}")))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| TrustError::Io(format!("{error}")))?;
    }
    fs::write(path, format!("{content}\n")).map_err(|error| TrustError::Io(format!("{error}")))
}

fn acquire_trust_lock(path: &Path) -> Result<LockGuard, TrustError> {
    let trust_dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(trust_dir).map_err(|error| TrustError::Io(format!("{error}")))?;
    let options = LockOptions {
        retries: 9,
        realpath: false,
        lockfile_path: Some(PathBuf::from(format!("{}.lock", path.display()))),
        ..LockOptions::default()
    };
    lock_sync(path, &options).map_err(|error| {
        if error == "ELOCKED" {
            TrustError::Lock("Failed to acquire trust store lock".to_string())
        } else {
            TrustError::Lock(error)
        }
    })
}

fn with_trust_file_lock<T>(
    path: &Path,
    action: impl FnOnce() -> Result<T, TrustError>,
) -> Result<T, TrustError> {
    let _lock = acquire_trust_lock(path)?;
    action()
}

/// True when `cwd` has project-local resources that require a trust decision.
pub fn has_trust_requiring_project_resources(cwd: &Path) -> bool {
    has_trust_requiring_project_resources_with_home(cwd, &home_dir())
}

pub fn has_trust_requiring_project_resources_with_home(cwd: &Path, home: &Path) -> bool {
    let user_agents_skills_dir = canonicalize_path(&home.join(".agents").join("skills"));
    let mut current = normalize_cwd(cwd);

    let config_dir = current.join(CONFIG_DIR_NAME);
    if TRUST_REQUIRING_PROJECT_CONFIG_RESOURCES
        .iter()
        .any(|entry| config_dir.join(entry).exists())
    {
        return true;
    }

    loop {
        let agents_skills_dir = current.join(".agents").join("skills");
        if !paths_equal(&agents_skills_dir, &user_agents_skills_dir) && agents_skills_dir.exists() {
            return true;
        }
        let Some(parent) = current.parent() else {
            return false;
        };
        if parent == current {
            return false;
        }
        current = parent.to_path_buf();
    }
}

#[derive(Debug, Clone)]
pub struct ProjectTrustStore {
    trust_path: PathBuf,
}

impl ProjectTrustStore {
    pub fn new(agent_dir: &Path) -> Self {
        Self {
            trust_path: resolve_path_default(agent_dir).join("trust.json"),
        }
    }

    pub fn trust_path(&self) -> &Path {
        &self.trust_path
    }

    pub fn get(&self, cwd: &Path) -> Result<ProjectTrustDecision, TrustError> {
        Ok(self.get_entry(cwd)?.map(|entry| entry.decision))
    }

    pub fn get_entry(&self, cwd: &Path) -> Result<Option<ProjectTrustStoreEntry>, TrustError> {
        with_trust_file_lock(&self.trust_path, || {
            let data = read_trust_file(&self.trust_path)?;
            Ok(find_nearest_trust_entry(&data, cwd))
        })
    }

    pub fn set(&self, cwd: &Path, decision: ProjectTrustDecision) -> Result<(), TrustError> {
        self.set_many(&[ProjectTrustUpdate {
            path: cwd.to_string_lossy().into_owned(),
            decision,
        }])
    }

    /// `setMany` in upstream: equal keys are removed, null deletes instead of storing.
    pub fn set_many(&self, decisions: &[ProjectTrustUpdate]) -> Result<(), TrustError> {
        with_trust_file_lock(&self.trust_path, || {
            let mut data = read_trust_file(&self.trust_path)?;
            for update in decisions {
                let key = normalize_cwd(Path::new(&update.path))
                    .to_string_lossy()
                    .into_owned();
                for existing in find_trust_file_keys(&data, Path::new(&key)) {
                    data.shift_remove(&existing);
                }
                if update.decision.is_some() {
                    data.insert(key, update.decision);
                }
            }
            write_trust_file(&self.trust_path, &data)
        })
    }
}

pub fn format_project_trust_prompt(cwd: &Path) -> String {
    format!(
        "Trust project folder?\n{}\n\nThis allows {APP_NAME} to load {CONFIG_DIR_NAME} settings and resources, install missing project packages, and execute project extensions.",
        cwd.display()
    )
}

#[derive(Debug, Clone)]
pub struct ExtensionTrustError {
    pub extension_path: String,
    pub error: String,
}

#[derive(Debug, Clone)]
pub struct ExtensionTrustResult {
    pub trusted: String,
    pub remember: bool,
    pub errors: Vec<ExtensionTrustError>,
}

#[derive(Clone, Default)]
pub struct ResolveProjectTrustedOptions<'a> {
    pub cwd: Option<&'a Path>,
    pub trust_override: Option<bool>,
    pub default_project_trust: Option<DefaultProjectTrust>,
    pub extension_result: Option<ExtensionTrustResult>,
    pub on_extension_error: Option<&'a dyn Fn(&str)>,
    pub ui_decision: Option<String>,
    pub has_ui: bool,
    pub mode: Option<AppMode>,
    pub home_dir: Option<PathBuf>,
}

/// Pure decision logic. UI selection is passed in as `ui_decision` (the chosen label).
pub fn resolve_project_trusted(
    trust_store: &ProjectTrustStore,
    options: &ResolveProjectTrustedOptions<'_>,
) -> Result<bool, TrustError> {
    let cwd = options.cwd.unwrap_or_else(|| Path::new("."));
    if let Some(override_decision) = options.trust_override {
        return Ok(override_decision);
    }

    let needs_resources = options
        .home_dir
        .as_deref()
        .is_some_and(|home| has_trust_requiring_project_resources_with_home(cwd, home))
        || options.home_dir.is_none() && has_trust_requiring_project_resources(cwd);
    if !needs_resources {
        return Ok(trust_store.get(cwd)?.unwrap_or(true));
    }

    if let Some(result) = &options.extension_result {
        for error in &result.errors {
            if let Some(callback) = options.on_extension_error {
                callback(&format!(
                    "Extension \"{}\" project_trust error: {}",
                    error.extension_path, error.error
                ));
            }
        }
        let trusted = result.trusted == "yes";
        if result.remember {
            trust_store.set(cwd, Some(trusted))?;
        }
        return Ok(trusted);
    }

    if let Some(decision) = trust_store.get(cwd)? {
        return Ok(decision);
    }

    match options
        .default_project_trust
        .unwrap_or(DefaultProjectTrust::Ask)
    {
        DefaultProjectTrust::Always => return Ok(true),
        DefaultProjectTrust::Never => return Ok(false),
        DefaultProjectTrust::Ask => {}
    }

    if !options.has_ui {
        return Ok(false);
    }

    let selected_label = options.ui_decision.as_deref();
    let options_list = get_project_trust_options(cwd, true);
    if let Some(label) = selected_label
        && let Some(selected) = options_list.iter().find(|option| option.label == label)
    {
        if !selected.updates.is_empty() {
            trust_store.set_many(&selected.updates)?;
        }
        return Ok(selected.trusted);
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_dir(dir: &Path) -> PathBuf {
        dir.join("agent")
    }

    #[test]
    fn trust_inheritance_and_removal() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| format!("tempdir: {error}"))?;
        let store = ProjectTrustStore::new(&agent_dir(dir.path()));
        let parent = dir.path().join("parent");
        let child = parent.join("child");
        let parent_string = parent.to_string_lossy().into_owned();
        let child_string = child.to_string_lossy().into_owned();

        assert_eq!(store.get(&child).map_err(|error| error.to_string())?, None);
        store
            .set(&parent, Some(true))
            .map_err(|error| error.to_string())?;
        assert_eq!(
            store.get(&child).map_err(|error| error.to_string())?,
            Some(true)
        );
        store
            .set(&child, Some(false))
            .map_err(|error| error.to_string())?;
        assert_eq!(
            store.get(&child).map_err(|error| error.to_string())?,
            Some(false)
        );
        store.set(&child, None).map_err(|error| error.to_string())?;
        assert_eq!(
            store.get(&child).map_err(|error| error.to_string())?,
            Some(true)
        );

        let data = read_trust_file(store.trust_path()).map_err(|error| error.to_string())?;
        assert!(data.contains_key(&parent_string));
        assert!(!data.contains_key(&child_string));
        Ok(())
    }

    #[test]
    fn write_trust_file_is_sorted_and_uses_two_space_indent() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| format!("tempdir: {error}"))?;
        let path = dir.path().join("agent").join("trust.json");
        let mut data = TrustFile::new();
        data.insert("/b".to_string(), Some(false));
        data.insert("/a".to_string(), Some(true));
        data.insert("/c".to_string(), None);
        data.insert("/d".to_string(), Some(true));
        write_trust_file(&path, &data).map_err(|error| error.to_string())?;
        let content = fs::read_to_string(&path).map_err(|error| format!("read: {error}"))?;
        assert_eq!(
            content,
            "{\n  \"/a\": true,\n  \"/b\": false,\n  \"/c\": null,\n  \"/d\": true\n}\n"
        );
        Ok(())
    }

    #[test]
    fn read_trust_file_error_messages() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| format!("tempdir: {error}"))?;
        let invalid = dir.path().join("invalid.json");
        fs::write(&invalid, "{").map_err(|error| format!("write: {error}"))?;
        let error = read_trust_file(&invalid).expect_err("invalid JSON");
        assert!(error.to_string().starts_with(&format!(
            "Failed to read trust store {}:",
            invalid.display()
        )));

        let wrong = dir.path().join("wrong.json");
        fs::write(&wrong, "[]").map_err(|error| format!("write: {error}"))?;
        let error = read_trust_file(&wrong).expect_err("array");
        assert_eq!(
            error.to_string(),
            format!(
                "Invalid trust store {}: expected an object",
                wrong.display()
            )
        );

        let bad_value = dir.path().join("bad-value.json");
        fs::write(&bad_value, "{\"/x\": 1}").map_err(|error| format!("write: {error}"))?;
        let error = read_trust_file(&bad_value).expect_err("number value");
        assert!(
            error
                .to_string()
                .contains("value for \"/x\" must be true, false, or null")
        );
        Ok(())
    }

    #[test]
    fn trust_requiring_resources_detection() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| format!("tempdir: {error}"))?;
        let home = dir.path().join("home");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).map_err(|error| format!("mkdir: {error}"))?;
        assert!(!has_trust_requiring_project_resources_with_home(
            &project, &home
        ));

        for resource in TRUST_REQUIRING_PROJECT_CONFIG_RESOURCES {
            let path = project.join(CONFIG_DIR_NAME).join(resource);
            if resource.ends_with(".json") || resource.ends_with(".md") {
                write_file(&path)?;
            } else {
                fs::create_dir_all(&path).map_err(|error| format!("mkdir: {error}"))?;
            }
            assert!(has_trust_requiring_project_resources_with_home(
                &project, &home
            ));
            fs::remove_file(&path)
                .or_else(|_| fs::remove_dir_all(&path))
                .map_err(|error| format!("remove: {error}"))?;
        }

        let ancestor_skills = dir.path().join(".agents").join("skills");
        fs::create_dir_all(&ancestor_skills).map_err(|error| format!("mkdir: {error}"))?;
        assert!(has_trust_requiring_project_resources_with_home(
            &project, &home
        ));

        let user_skills = home.join(".agents").join("skills");
        fs::create_dir_all(&user_skills).map_err(|error| format!("mkdir: {error}"))?;
        assert!(has_trust_requiring_project_resources_with_home(
            &project, &home
        ));
        Ok(())
    }

    fn write_file(path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| format!("mkdir: {error}"))?;
        }
        fs::write(path, "").map_err(|error| format!("write: {error}"))
    }

    #[test]
    fn user_agents_skills_dir_is_not_project_resource() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| format!("tempdir: {error}"))?;
        let home = dir.path().join("home");
        let user_skills = home.join(".agents").join("skills");
        fs::create_dir_all(&user_skills).map_err(|error| format!("mkdir: {error}"))?;
        assert!(!has_trust_requiring_project_resources_with_home(
            &home, &home
        ));
        Ok(())
    }

    #[test]
    fn project_trust_options_and_parent_update() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| format!("tempdir: {error}"))?;
        let project = dir.path().join("project");
        fs::create_dir_all(&project).map_err(|error| format!("mkdir: {error}"))?;
        let options = get_project_trust_options(&project, true);
        assert_eq!(options.len(), 5);
        assert_eq!(options[0].label, "Trust");
        assert!(options[1].label.starts_with("Trust parent folder ("));
        assert_eq!(options[2].label, "Trust (this session only)");
        assert_eq!(options[3].label, "Do not trust");
        assert_eq!(options[4].label, "Do not trust (this session only)");
        assert_eq!(options[1].updates.len(), 2);
        assert_eq!(options[1].updates[1].decision, None);
        assert!(options[2].updates.is_empty());
        Ok(())
    }

    #[test]
    fn resolve_override_and_saved_decisions() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| format!("tempdir: {error}"))?;
        let store = ProjectTrustStore::new(&agent_dir(dir.path()));
        let project = dir.path().join("project");
        fs::create_dir_all(&project).map_err(|error| format!("mkdir: {error}"))?;
        let options = ResolveProjectTrustedOptions {
            cwd: Some(&project),
            trust_override: Some(false),
            ..Default::default()
        };
        assert!(!resolve_project_trusted(&store, &options).map_err(|error| error.to_string())?);

        // No trust-requiring resources: saved decision wins, otherwise auto-trust.
        let options = ResolveProjectTrustedOptions {
            cwd: Some(&project),
            ..Default::default()
        };
        assert!(resolve_project_trusted(&store, &options).map_err(|error| error.to_string())?);
        store
            .set(&project, Some(false))
            .map_err(|error| error.to_string())?;
        assert!(!resolve_project_trusted(&store, &options).map_err(|error| error.to_string())?);
        store
            .set(&project, Some(true))
            .map_err(|error| error.to_string())?;
        assert!(resolve_project_trusted(&store, &options).map_err(|error| error.to_string())?);
        Ok(())
    }

    #[test]
    fn resolve_defaults_and_ui_selection() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| format!("tempdir: {error}"))?;
        let store = ProjectTrustStore::new(&agent_dir(dir.path()));
        let home = dir.path().join("home");
        let project = dir.path().join("project");
        fs::create_dir_all(project.join(CONFIG_DIR_NAME))
            .map_err(|error| format!("mkdir: {error}"))?;
        write_file(&project.join(CONFIG_DIR_NAME).join("settings.json"))?;

        let base = ResolveProjectTrustedOptions {
            cwd: Some(&project),
            home_dir: Some(home.clone()),
            ..Default::default()
        };
        let mut always = base.clone();
        always.default_project_trust = Some(DefaultProjectTrust::Always);
        assert!(resolve_project_trusted(&store, &always).map_err(|error| error.to_string())?);
        let mut never = base.clone();
        never.default_project_trust = Some(DefaultProjectTrust::Never);
        assert!(!resolve_project_trusted(&store, &never).map_err(|error| error.to_string())?);

        let mut ask_without_ui = base.clone();
        ask_without_ui.default_project_trust = Some(DefaultProjectTrust::Ask);
        assert!(
            !resolve_project_trusted(&store, &ask_without_ui).map_err(|error| error.to_string())?
        );

        let mut with_ui = base.clone();
        with_ui.has_ui = true;
        with_ui.ui_decision = Some("Do not trust".to_string());
        assert!(!resolve_project_trusted(&store, &with_ui).map_err(|error| error.to_string())?);
        assert_eq!(
            store.get(&project).map_err(|error| error.to_string())?,
            Some(false)
        );
        Ok(())
    }

    #[test]
    fn resolve_extension_result_and_prompt_text() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| format!("tempdir: {error}"))?;
        let store = ProjectTrustStore::new(&agent_dir(dir.path()));
        let project = dir.path().join("project");
        write_file(&project.join(CONFIG_DIR_NAME).join("settings.json"))?;
        let mut options = ResolveProjectTrustedOptions {
            cwd: Some(&project),
            extension_result: Some(ExtensionTrustResult {
                trusted: "yes".to_string(),
                remember: true,
                errors: vec![ExtensionTrustError {
                    extension_path: "/tmp/ext".to_string(),
                    error: "boom".to_string(),
                }],
            }),
            ..Default::default()
        };
        let messages = std::cell::RefCell::new(Vec::new());
        let collect_message = |message: &str| {
            messages.borrow_mut().push(message.to_string());
        };
        options.on_extension_error = Some(&collect_message);
        assert!(resolve_project_trusted(&store, &options).map_err(|error| error.to_string())?);
        assert_eq!(
            store.get(&project).map_err(|error| error.to_string())?,
            Some(true)
        );
        assert_eq!(
            messages.borrow()[0],
            "Extension \"/tmp/ext\" project_trust error: boom"
        );
        let prompt = format_project_trust_prompt(&project);
        assert!(prompt.starts_with("Trust project folder?\n"));
        assert!(prompt.contains("This allows pi to load .pi settings and resources"));
        Ok(())
    }

    #[test]
    fn canonicalization_and_nearest_entry() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| format!("tempdir: {error}"))?;
        let path = dir.path().join("a").join("b");
        fs::create_dir_all(&path).map_err(|error| format!("mkdir: {error}"))?;
        assert_eq!(normalize_cwd(&path), canonicalize_path(&path));
        let mut data = TrustFile::new();
        data.insert(path.to_string_lossy().into_owned(), Some(false));
        let entry = find_nearest_trust_entry(&data, &path).ok_or("missing entry")?;
        assert!(!entry.decision);
        Ok(())
    }
}
