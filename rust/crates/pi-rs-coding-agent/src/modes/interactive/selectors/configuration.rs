use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use globset::GlobBuilder;
use pi_rs_tui::{Component, Focusable, get_keybindings};
use serde_json::{Value, json};

use super::Picker;
use crate::config::CONFIG_DIR_NAME;
use crate::core::package_manager::{
    InstallState, PackageManager, PackageScope, PackageSource, ResolveResult, ResolvedResource,
    ResourceKind,
};
use crate::core::settings_manager::{SettingsManager, SettingsScope};
use crate::core::skills::{canonicalize_path, path_comparison_key};
use crate::modes::interactive::theme::Theme;
use crate::utils::paths::resolve_path;

#[derive(Clone, Debug)]
pub struct ConfigResource {
    pub path: PathBuf,
    pub kind: ResourceKind,
    pub source: String,
    pub scope: PackageScope,
    pub enabled: bool,
    package_root: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectResourceOverride {
    Inherit,
    Load,
    Unload,
}

impl ProjectResourceOverride {
    fn cycle(self, inherited: bool) -> Self {
        use ProjectResourceOverride::{Inherit, Load, Unload};
        match (self, inherited) {
            (Inherit, true) | (Load, false) => Unload,
            (Inherit, false) | (Unload, true) => Load,
            (Load, true) | (Unload, false) => Inherit,
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct ConfigEdit {
    pub scope: SettingsScope,
    pub key: String,
    pub value: Value,
}

const KINDS: [ResourceKind; 4] = [
    ResourceKind::Extension,
    ResourceKind::Skill,
    ResourceKind::Prompt,
    ResourceKind::Theme,
];

fn kind_key(kind: ResourceKind) -> &'static str {
    match kind {
        ResourceKind::Extension => "extensions",
        ResourceKind::Skill => "skills",
        ResourceKind::Prompt => "prompts",
        ResourceKind::Theme => "themes",
    }
}

fn kind_index(kind: ResourceKind) -> usize {
    KINDS
        .iter()
        .position(|item| *item == kind)
        .unwrap_or_default()
}

fn scope_index(scope: PackageScope) -> usize {
    match scope {
        PackageScope::User => 0,
        PackageScope::Project => 1,
        PackageScope::Temporary => 2,
    }
}

impl ConfigResource {
    fn from_resolved(resource: ResolvedResource, resolved: &ResolveResult) -> Self {
        let package_root = resolved
            .packages
            .iter()
            .find(|package| {
                package.source == resource.source
                    && package.scope == resource.scope
                    && resource.path.starts_with(&package.path)
            })
            .map(|package| package.path.clone());
        Self {
            path: resource.path,
            kind: resource.kind,
            source: resource.source,
            scope: resource.scope,
            enabled: resource.enabled,
            package_root,
        }
    }

    fn is_builtin(&self) -> bool {
        self.source == "builtin"
    }
    fn display_name(&self) -> String {
        let path = self.path.to_string_lossy();
        if self.is_builtin() {
            return path.strip_prefix("builtin:").unwrap_or(&path).into();
        }
        let base = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&path);
        let parent = self
            .path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        match self.kind {
            ResourceKind::Skill if base == "SKILL.md" && !parent.is_empty() => parent.to_owned(),
            ResourceKind::Extension if !parent.is_empty() && parent != "extensions" => {
                format!("{parent}/{base}")
            }
            _ => base.to_owned(),
        }
    }
}

fn normalized(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn relative(base: &Path, path: &Path) -> String {
    if !path.is_absolute() || !base.is_absolute() {
        return normalized(path);
    }
    let mut base = base.components().peekable();
    let mut path_components = path.components().peekable();
    if base.peek() != path_components.peek() {
        return normalized(path);
    }
    while base.peek().is_some() && base.peek() == path_components.peek() {
        base.next();
        path_components.next();
    }
    let result: PathBuf = std::iter::repeat_n(std::path::Component::ParentDir, base.count())
        .chain(path_components)
        .collect();
    normalized(&result)
}

fn marker(entry: &str) -> (Option<u8>, &str) {
    match entry.as_bytes().first() {
        Some(&prefix @ (b'!' | b'+' | b'-')) => (Some(prefix), &entry[1..]),
        _ => (None, entry),
    }
}

fn matches_pattern(pattern: &str, candidates: &[String]) -> bool {
    if candidates.iter().any(|candidate| candidate == pattern) {
        return true;
    }
    if !pattern.contains(['*', '?', '[', '{']) {
        return false;
    }
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .is_ok_and(|glob| {
            let matcher = glob.compile_matcher();
            candidates
                .iter()
                .any(|candidate| matcher.is_match(candidate))
        })
}

fn path_rules(value: &Value) -> impl DoubleEndedIterator<Item = &str> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
}

fn resource_override(
    value: &Value,
    candidates: &[String],
    empty_unloads: bool,
) -> ProjectResourceOverride {
    if empty_unloads && value.as_array().is_some_and(Vec::is_empty) {
        return ProjectResourceOverride::Unload;
    }
    for entry in path_rules(value).rev() {
        let (prefix, pattern) = marker(entry);
        if matches_pattern(pattern, candidates) {
            return if matches!(prefix, Some(b'-' | b'!')) {
                ProjectResourceOverride::Unload
            } else {
                ProjectResourceOverride::Load
            };
        }
    }
    if empty_unloads && path_rules(value).any(|entry| marker(entry).0.is_none()) {
        ProjectResourceOverride::Unload
    } else {
        ProjectResourceOverride::Inherit
    }
}

fn source_value(package: &Value) -> Option<&str> {
    package.as_str().or_else(|| package["source"].as_str())
}

fn source_matches(left: &str, left_base: &Path, right: &str, right_base: &Path) -> bool {
    if left == right && left_base == right_base {
        return true;
    }
    match (PackageSource::parse(left), PackageSource::parse(right)) {
        (Ok(PackageSource::Local { .. }), Ok(PackageSource::Local { .. })) => {
            let left = canonicalize_path(&resolve_path(left, left_base, true));
            let right = canonicalize_path(&resolve_path(right, right_base, true));
            path_comparison_key(&left) == path_comparison_key(&right)
        }
        _ => left == right,
    }
}

fn allowed_file(kind: ResourceKind, path: &Path) -> bool {
    let extension = path.extension().and_then(|extension| extension.to_str());
    match kind {
        ResourceKind::Extension => matches!(extension, Some("wasm" | "ts" | "js")),
        ResourceKind::Skill | ResourceKind::Prompt => extension == Some("md"),
        ResourceKind::Theme => extension == Some("json"),
    }
}

fn files(root: &Path, kind: ResourceKind) -> Vec<PathBuf> {
    if root.is_file() {
        return allowed_file(kind, root)
            .then(|| root.to_path_buf())
            .into_iter()
            .collect();
    }
    let mut files = Vec::new();
    let mut folders = vec![root.to_path_buf()];
    while let Some(folder) = folders.pop() {
        if let Ok(entries) = std::fs::read_dir(&folder) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Ok(file_type) = entry.file_type() {
                    if file_type.is_dir() {
                        folders.push(path);
                    } else if (file_type.is_file() || file_type.is_symlink())
                        && allowed_file(kind, &path)
                    {
                        if kind == ResourceKind::Skill
                            && folder != root
                            && path.file_name().is_none_or(|name| name != "SKILL.md")
                        {
                            continue;
                        }
                        files.push(path);
                    }
                }
            }
        }
    }
    files.sort();
    files
}

fn discover_top_level(
    resources: &mut Vec<ConfigResource>,
    base: &Path,
    settings: &Value,
    scope: PackageScope,
) {
    for kind in KINDS {
        let key = kind_key(kind);
        let paths =
            std::iter::once(base.join(key)).chain(path_rules(&settings[key]).filter_map(|entry| {
                let (prefix, path) = marker(entry);
                (prefix.is_none() && !path.starts_with("builtin:"))
                    .then(|| resolve_path(path, base, true))
            }));
        for root in paths {
            for path in files(&root, kind) {
                resources.push(ConfigResource {
                    path,
                    kind,
                    source: "auto".into(),
                    scope,
                    enabled: true,
                    package_root: None,
                });
            }
        }
    }
}

pub fn config_resources(
    mut resolved: ResolveResult,
    settings: &SettingsManager,
    cwd: &Path,
    agent_dir: &Path,
    project_available: bool,
) -> Vec<ConfigResource> {
    let provided = std::mem::take(&mut resolved.resources);
    let mut resources: Vec<_> = provided
        .into_iter()
        .filter(|resource| project_available || resource.scope != PackageScope::Project)
        .map(|resource| ConfigResource::from_resolved(resource, &resolved))
        .collect();
    resources.extend(
        crate::extensions::builtin_extensions()
            .into_iter()
            .map(|extension| ConfigResource {
                path: PathBuf::from(format!("builtin:{}", extension.name)),
                kind: ResourceKind::Extension,
                source: "builtin".into(),
                scope: PackageScope::User,
                enabled: true,
                package_root: None,
            }),
    );
    let global = settings.get_global_settings();
    let project = if project_available {
        settings.get_project_settings()
    } else {
        Value::Null
    };
    let project_dir = cwd.join(CONFIG_DIR_NAME);
    discover_top_level(&mut resources, agent_dir, &global, PackageScope::User);
    if project_available {
        discover_top_level(
            &mut resources,
            &project_dir,
            &project,
            PackageScope::Project,
        );
    }

    let manager = PackageManager::with_settings(cwd, agent_dir, global, project).with_offline(true);
    // Resolve each scope independently: a project package override does not hide the global package in its own view.
    let mut packages = resolved.packages;
    let global_manager =
        PackageManager::with_settings(cwd, agent_dir, settings.get_global_settings(), Value::Null)
            .with_offline(true);
    packages.extend(global_manager.resolve_all().packages);
    if project_available {
        packages.extend(manager.resolve_all().packages);
    }
    let mut seen_packages = std::collections::HashSet::new();
    for package in packages {
        if package.state != InstallState::Installed
            || (!project_available && package.scope == PackageScope::Project)
            || !seen_packages.insert((
                package.source.clone(),
                scope_index(package.scope),
                package.path.clone(),
            ))
        {
            continue;
        }
        let root = package.path;
        let source = package.source;
        let mut discovered = manager
            .resolve_extension_sources(&[root.to_string_lossy().into_owned()])
            .resources;
        discovered.extend(
            files(&root.join("extensions"), ResourceKind::Extension)
                .into_iter()
                .filter(|path| {
                    path.extension()
                        .is_some_and(|extension| extension == "wasm")
                })
                .map(|path| ResolvedResource {
                    kind: ResourceKind::Extension,
                    path,
                    enabled: true,
                    source: source.clone(),
                    scope: package.scope,
                }),
        );
        resources.extend(discovered.into_iter().map(|resource| ConfigResource {
            path: resource.path,
            kind: resource.kind,
            source: source.clone(),
            scope: package.scope,
            enabled: resource.enabled,
            package_root: Some(root.clone()),
        }));
    }
    let mut seen = std::collections::HashSet::new();
    resources.retain(|resource| {
        seen.insert((
            kind_index(resource.kind),
            scope_index(resource.scope),
            path_comparison_key(&canonicalize_path(&resource.path)),
        ))
    });
    resources.sort_by_cached_key(|resource| {
        (
            resource.package_root.is_none(),
            scope_index(resource.scope),
            resource.source.to_lowercase(),
            kind_index(resource.kind),
            resource.display_name().to_lowercase(),
        )
    });
    resources
}

struct ConfigState {
    resources: Vec<ConfigResource>,
    global: Value,
    project: Value,
    agent_dir: PathBuf,
    project_dir: PathBuf,
}

impl ConfigState {
    fn base(&self, scope: PackageScope) -> &Path {
        if scope == PackageScope::Project {
            &self.project_dir
        } else {
            &self.agent_dir
        }
    }

    fn settings(&self, scope: SettingsScope) -> &Value {
        match scope {
            SettingsScope::Global => &self.global,
            SettingsScope::Project => &self.project,
        }
    }

    fn candidates(&self, resource: &ConfigResource, scope: SettingsScope) -> Vec<String> {
        if resource.is_builtin() {
            return vec![resource.path.to_string_lossy().into_owned()];
        }
        if let Some(root) = &resource.package_root {
            return vec![relative(root, &resource.path)];
        }
        let base = if scope == SettingsScope::Project {
            &self.project_dir
        } else {
            &self.agent_dir
        };
        vec![
            normalized(&resource.path),
            relative(base, &resource.path),
            relative(self.base(resource.scope), &resource.path),
        ]
    }

    fn find_package(
        &self,
        resource: &ConfigResource,
        scope: SettingsScope,
        packages: &[Value],
    ) -> Option<usize> {
        let target_base = if scope == SettingsScope::Project {
            &self.project_dir
        } else {
            &self.agent_dir
        };
        packages.iter().position(|package| {
            source_value(package).is_some_and(|source| {
                source_matches(
                    &resource.source,
                    self.base(resource.scope),
                    source,
                    target_base,
                )
            })
        })
    }

    fn state(&self, resource: &ConfigResource, scope: SettingsScope) -> ProjectResourceOverride {
        let settings = self.settings(scope);
        let key = kind_key(resource.kind);
        let candidates = self.candidates(resource, scope);
        if resource.package_root.is_some() {
            let Some(packages) = settings["packages"].as_array() else {
                return ProjectResourceOverride::Inherit;
            };
            let Some(index) = self.find_package(resource, scope, packages) else {
                return ProjectResourceOverride::Inherit;
            };
            let package = &packages[index];
            resource_override(
                &package[key],
                &candidates,
                package["autoload"].as_bool() != Some(false),
            )
        } else {
            resource_override(&settings[key], &candidates, false)
        }
    }

    fn inherited(&self, resource: &ConfigResource) -> bool {
        match self.state(resource, SettingsScope::Global) {
            ProjectResourceOverride::Load => true,
            ProjectResourceOverride::Unload => false,
            ProjectResourceOverride::Inherit => resource.enabled,
        }
    }

    fn edit(&self, resource: &ConfigResource, scope: SettingsScope) -> Option<ConfigEdit> {
        if scope == SettingsScope::Global && resource.scope != PackageScope::User {
            return None;
        }
        let settings = self.settings(scope);
        let inherited = self.inherited(resource);
        let next = if scope == SettingsScope::Global {
            if inherited {
                ProjectResourceOverride::Unload
            } else {
                ProjectResourceOverride::Load
            }
        } else {
            self.state(resource, scope).cycle(inherited)
        };
        let key = kind_key(resource.kind);
        let prefix = match next {
            ProjectResourceOverride::Load => Some('+'),
            ProjectResourceOverride::Unload => Some('-'),
            ProjectResourceOverride::Inherit => None,
        };
        let candidates = self.candidates(resource, scope);
        if let Some(root) = &resource.package_root {
            let pattern = relative(root, &resource.path);
            let mut packages = settings["packages"].as_array().cloned().unwrap_or_default();
            let index = match self.find_package(resource, scope, &packages) {
                Some(index) => index,
                None if scope == SettingsScope::Project && prefix.is_some() => {
                    let source = if matches!(
                        PackageSource::parse(&resource.source),
                        Ok(PackageSource::Local { .. })
                    ) {
                        let path = resolve_path(&resource.source, self.base(resource.scope), true);
                        let local = relative(&self.project_dir, &path);
                        if local.is_empty() { ".".into() } else { local }
                    } else {
                        resource.source.clone()
                    };
                    packages.push(json!({"source": source, "autoload": false}));
                    packages.len() - 1
                }
                None => return None,
            };
            let package = &mut packages[index];
            if let Some(source) = package.as_str() {
                *package = json!({"source": source});
            }
            let filters = package.as_object_mut()?;
            let mut values: Vec<_> = filters
                .get(key)
                .into_iter()
                .flat_map(path_rules)
                .filter(|entry| marker(entry).1 != pattern)
                .map(str::to_owned)
                .collect();
            if let Some(prefix) = prefix {
                values.push(format!("{prefix}{pattern}"));
            }
            if values.is_empty() {
                filters.remove(key);
            } else {
                filters.insert(key.into(), json!(values));
            }
            if !KINDS
                .iter()
                .any(|&kind| filters.contains_key(kind_key(kind)))
            {
                if filters.get("autoload").and_then(Value::as_bool) == Some(false) {
                    packages.remove(index);
                } else if let Some(source) = source_value(package) {
                    *package = json!(source);
                }
            }
            Some(ConfigEdit {
                scope,
                key: "packages".into(),
                value: json!(packages),
            })
        } else {
            let global_reference = scope == SettingsScope::Project
                && resource.scope == PackageScope::User
                && !resource.is_builtin();
            let pattern = if global_reference || resource.is_builtin() {
                &candidates[0]
            } else {
                &candidates[1]
            };
            let mut values: Vec<_> = path_rules(&settings[key])
                .filter(|entry| {
                    let (marker, target) = marker(entry);
                    !((marker.is_some() && candidates.iter().any(|candidate| candidate == target))
                        || (global_reference && prefix.is_none() && target == pattern))
                })
                .map(str::to_owned)
                .collect();
            if let Some(prefix) = prefix {
                if global_reference && !values.contains(pattern) {
                    values.push(pattern.clone());
                }
                values.push(format!("{prefix}{pattern}"));
            }
            Some(ConfigEdit {
                scope,
                key: key.to_owned(),
                value: json!(values),
            })
        }
    }
}

/// Effective global/project resource rules. Discover once per reload, then use the
/// same policy for extension, skill, prompt and theme paths. Caller-owned CLI
/// paths bypass the filter because explicit command-line requests have priority.
pub struct ConfiguredResources {
    state: ConfigState,
    by_path: HashMap<(usize, String), usize>,
    project_trusted: bool,
}

fn resource_path_key(path: &Path) -> String {
    if path.to_string_lossy().starts_with("builtin:") {
        path.to_string_lossy().into_owned()
    } else {
        path_comparison_key(&canonicalize_path(path))
    }
}

impl ConfiguredResources {
    pub fn new(settings: &SettingsManager, cwd: &Path, agent_dir: &Path) -> Self {
        let project_trusted = settings.is_project_trusted();
        let resources = config_resources(
            ResolveResult::default(),
            settings,
            cwd,
            agent_dir,
            project_trusted,
        );
        let mut by_path = HashMap::with_capacity(resources.len());
        for (index, resource) in resources.iter().enumerate() {
            let key = (kind_index(resource.kind), resource_path_key(&resource.path));
            // An inherited resource named explicitly in project settings is still
            // global. Keep its original source and package identity.
            by_path.entry(key).or_insert(index);
        }
        Self {
            state: ConfigState {
                resources,
                global: settings.get_global_settings(),
                project: if project_trusted {
                    settings.get_project_settings()
                } else {
                    Value::Null
                },
                agent_dir: agent_dir.to_path_buf(),
                project_dir: cwd.join(CONFIG_DIR_NAME),
            },
            by_path,
            project_trusted,
        }
    }

    pub fn enabled(&self, kind: ResourceKind, path: &Path, default: bool) -> bool {
        let key = (kind_index(kind), resource_path_key(path));
        if let Some(&index) = self.by_path.get(&key) {
            return self.is_enabled(&self.state.resources[index]);
        }
        let scope = if path.starts_with(&self.state.project_dir) {
            PackageScope::Project
        } else {
            PackageScope::User
        };
        let fallback = ConfigResource {
            path: path.to_path_buf(),
            kind,
            scope,
            enabled: default,
            source: if key.1.starts_with("builtin:") {
                "builtin".into()
            } else {
                "auto".into()
            },
            package_root: None,
        };
        self.is_enabled(&fallback)
    }

    pub fn enabled_paths(&self, kind: ResourceKind) -> Vec<PathBuf> {
        let mut seen = HashSet::new();
        self.state
            .resources
            .iter()
            .filter_map(|resource| {
                if resource.kind != kind {
                    return None;
                }
                let key = resource_path_key(&resource.path);
                if !seen.insert(key.clone()) {
                    return None;
                }
                let index = *self.by_path.get(&(kind_index(kind), key))?;
                let resource = &self.state.resources[index];
                self.is_enabled(resource).then(|| resource.path.clone())
            })
            .collect()
    }

    fn is_enabled(&self, resource: &ConfigResource) -> bool {
        if resource.scope == PackageScope::Project && !self.project_trusted {
            return false;
        }
        if self.project_trusted {
            match self.state.state(resource, SettingsScope::Project) {
                ProjectResourceOverride::Load => return true,
                ProjectResourceOverride::Unload => return false,
                ProjectResourceOverride::Inherit => {}
            }
        }
        if resource.scope == PackageScope::Project {
            resource.enabled
        } else {
            self.state.inherited(resource)
        }
    }
}

pub fn configured_resource_enabled(
    settings: &SettingsManager,
    cwd: &Path,
    agent_dir: &Path,
    kind: ResourceKind,
    path: &Path,
    default: bool,
) -> bool {
    ConfiguredResources::new(settings, cwd, agent_dir).enabled(kind, path, default)
}

pub struct ConfigPickerOptions<'a> {
    pub cwd: &'a Path,
    pub agent_dir: &'a Path,
    pub scope: SettingsScope,
    pub project_available: bool,
}

pub struct ConfigPicker {
    picker: Picker,
    state: ConfigState,
    scope: SettingsScope,
    project_available: bool,
    change: Box<dyn FnMut(Option<ConfigEdit>)>,
}

impl ConfigPicker {
    pub fn new(
        resources: Vec<ConfigResource>,
        settings: &SettingsManager,
        options: ConfigPickerOptions<'_>,
        theme: Theme,
        change: impl FnMut(Option<ConfigEdit>) + 'static,
    ) -> Self {
        let ConfigPickerOptions {
            cwd,
            agent_dir,
            scope,
            project_available,
        } = options;
        let mut dialog = Self {
            picker: Picker::new(String::new(), Vec::new(), theme, |_| {}).without_wrap(),
            state: ConfigState {
                resources,
                global: settings.get_global_settings(),
                project: settings.get_project_settings(),
                agent_dir: agent_dir.to_path_buf(),
                project_dir: cwd.join(CONFIG_DIR_NAME),
            },
            scope: if project_available {
                scope
            } else {
                SettingsScope::Global
            },
            project_available,
            change: Box::new(change),
        };
        dialog.refresh(None);
        dialog
    }

    fn refresh(&mut self, keep: Option<usize>) {
        let project = self.scope == SettingsScope::Project;
        self.picker.title = if project {
            "Project Local Resources"
        } else {
            "Global Resources"
        }
        .into();
        self.picker.description = if project {
            format!("{CONFIG_DIR_NAME}/settings.json · global resources can be overridden")
        } else {
            format!("~/{CONFIG_DIR_NAME}/agent/settings.json")
        };
        let switch = if self.project_available {
            " · Tab switch mode"
        } else {
            ""
        };
        let action = if project {
            "cycle inherit/+/-"
        } else {
            "toggle"
        };
        self.picker.hint = format!("Space/Enter {action}{switch} · Esc close");
        let mut rows = vec![String::new(); self.state.resources.len()];
        let mut included = Vec::new();
        for (index, resource) in self.state.resources.iter().enumerate() {
            if (!project && resource.scope != PackageScope::User)
                || resource.scope == PackageScope::Temporary
            {
                continue;
            }
            let inherited = self.state.inherited(resource);
            let checkbox = match (project, self.state.state(resource, self.scope)) {
                (true, ProjectResourceOverride::Load) => "[+]",
                (true, ProjectResourceOverride::Unload) => "[-]",
                _ if inherited => "[x]",
                _ => "[ ]",
            };
            let group = if resource.package_root.is_some() {
                format!("{} ({})", resource.source, resource.scope.as_str())
            } else if resource.is_builtin() {
                "Built-in".into()
            } else if resource.scope == PackageScope::Project {
                "Project".into()
            } else {
                "User".into()
            };
            let suffix = if project
                && resource.scope == PackageScope::User
                && checkbox != "[+]"
                && checkbox != "[-]"
            {
                " · inherited global"
            } else {
                ""
            };
            let name = resource.display_name();
            let search = format!(
                "{name} {} {}",
                kind_key(resource.kind),
                resource.path.display()
            )
            .to_lowercase();
            rows[index] = format!(
                "{checkbox} {name} · {} · {group}{suffix}",
                kind_key(resource.kind)
            );
            included.push((index, search));
        }
        self.picker.replace_rows(
            rows,
            Box::new(move |query| {
                let query = query.to_lowercase();
                included
                    .iter()
                    .filter(|(_, search)| query.trim().is_empty() || search.contains(&query))
                    .map(|(index, _)| *index)
                    .collect()
            }),
            keep,
        );
    }
}

impl Component for ConfigPicker {
    fn render(&self, width: usize) -> Vec<String> {
        self.picker.render(width)
    }
    fn handle_input(&mut self, data: &str) {
        let keys = get_keybindings();
        if keys.matches(data, "tui.input.tab") && self.project_available {
            self.scope = if self.scope == SettingsScope::Global {
                SettingsScope::Project
            } else {
                SettingsScope::Global
            };
            self.refresh(None);
        } else if keys.matches(data, "tui.select.cancel") {
            (self.change)(None);
        } else if data == " " || data == "\n" || keys.matches(data, "tui.select.confirm") {
            if let Some(index) = self.picker.selected_index()
                && let Some(edit) = self.state.edit(&self.state.resources[index], self.scope)
            {
                let snapshot = match edit.scope {
                    SettingsScope::Global => &mut self.state.global,
                    SettingsScope::Project => &mut self.state.project,
                };
                snapshot[&edit.key] = edit.value.clone();
                self.refresh(Some(index));
                (self.change)(Some(edit));
            }
        } else {
            self.picker.handle_input(data);
        }
    }
    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(&mut self.picker)
    }
    fn captures_viewport_input(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_cycles_against_global_and_does_not_mutate_global() {
        let resource = ConfigResource {
            path: PathBuf::from("builtin:mcp"),
            kind: ResourceKind::Extension,
            source: "builtin".into(),
            scope: PackageScope::User,
            enabled: true,
            package_root: None,
        };
        let mut state = ConfigState {
            resources: Vec::new(),
            global: json!({}),
            project: json!({}),
            agent_dir: PathBuf::from("/agent"),
            project_dir: PathBuf::from("/project/.pi"),
        };
        for expected in [json!(["-builtin:mcp"]), json!(["+builtin:mcp"]), json!([])] {
            let edit = state.edit(&resource, SettingsScope::Project);
            assert_eq!(edit.as_ref().map(|edit| &edit.value), Some(&expected));
            state.project["extensions"] = expected;
        }
        assert_eq!(state.global, json!({}));
        assert_eq!(
            state
                .edit(&resource, SettingsScope::Global)
                .map(|edit| edit.value),
            Some(json!(["-builtin:mcp"]))
        );
    }

    #[test]
    fn project_package_override_uses_relative_resource_and_drops_empty_override() {
        let resource = ConfigResource {
            path: PathBuf::from("/agent/pkg/themes/t.json"),
            kind: ResourceKind::Theme,
            source: "./pkg".into(),
            scope: PackageScope::User,
            enabled: true,
            package_root: Some(PathBuf::from("/agent/pkg")),
        };
        let mut state = ConfigState {
            resources: Vec::new(),
            global: json!({"packages":["./pkg"]}),
            project: json!({}),
            agent_dir: PathBuf::from("/agent"),
            project_dir: PathBuf::from("/project/.pi"),
        };
        for expected in [
            json!([{"source":"../../agent/pkg","autoload":false,"themes":["-themes/t.json"]}]),
            json!([{"source":"../../agent/pkg","autoload":false,"themes":["+themes/t.json"]}]),
            json!([]),
        ] {
            let edit = state.edit(&resource, SettingsScope::Project);
            assert_eq!(edit.as_ref().map(|edit| &edit.value), Some(&expected));
            state.project["packages"] = expected;
        }
        assert_eq!(state.global, json!({"packages":["./pkg"]}));
    }

    #[test]
    fn persisted_config_filters_global_and_trusted_project_resources() -> Result<(), String> {
        use crate::core::settings_manager::CreateOptions;
        let root = tempfile::tempdir().map_err(|error| error.to_string())?;
        let cwd = root.path().join("work");
        let agent = root.path().join("agent");
        let prompts = agent.join("prompts");
        std::fs::create_dir_all(&prompts).map_err(|error| error.to_string())?;
        let prompt = prompts.join("test.md");
        std::fs::write(&prompt, "test").map_err(|error| error.to_string())?;
        let trusted = CreateOptions {
            project_trusted: true,
        };
        let mut settings = SettingsManager::create_with_dirs(&cwd, &agent, trusted);
        settings.set_value(&["extensions".into()], json!(["-builtin:mcp"]))?;
        settings.set_value(&["prompts".into()], json!(["-prompts/test.md"]))?;
        settings.set_project_value(&["extensions".into()], json!(["+builtin:mcp"]))?;
        let reopened = SettingsManager::create_with_dirs(&cwd, &agent, trusted);
        let resources = ConfiguredResources::new(&reopened, &cwd, &agent);
        assert!(resources.enabled(ResourceKind::Extension, Path::new("builtin:mcp"), true));
        assert!(!resources.enabled(ResourceKind::Prompt, &prompt, true));
        assert!(resources.enabled_paths(ResourceKind::Prompt).is_empty());
        let untrusted = SettingsManager::create_with_dirs(
            &cwd,
            &agent,
            CreateOptions {
                project_trusted: false,
            },
        );
        assert!(!configured_resource_enabled(
            &untrusted,
            &cwd,
            &agent,
            ResourceKind::Extension,
            Path::new("builtin:mcp"),
            true
        ));
        Ok(())
    }
}
