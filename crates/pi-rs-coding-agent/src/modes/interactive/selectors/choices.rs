use pi_rs_ai::{
    Model, ThinkingLevel,
    auth::{AuthCheck, AuthType},
};
use pi_rs_tui::{Component, Focusable, get_keybindings, keys::matches_key_data};

use super::{Picker, PickerItem, fuzzy_rank};
use crate::modes::interactive::theme::Theme;
use crate::modes::interactive::utilities::{
    get_distinct_model_name, get_model_search_text, get_model_selector_search_text,
};

fn model_reference(model: &Model) -> (&str, &str) {
    (&model.provider, &model.id)
}

fn model_search(model: &Model) -> String {
    get_model_selector_search_text(&model.id, &model.provider, Some(&model.name))
}

fn model_label(model: &Model) -> String {
    let (provider, id) = model_reference(model);
    match get_distinct_model_name(id, Some(&model.name)) {
        Some(name) => format!("{id} ({name}) [{provider}]"),
        None => format!("{id} [{provider}]"),
    }
}

pub fn model_items(
    models: &[Model],
    current: Option<&Model>,
    default: Option<(&str, &str)>,
) -> Vec<PickerItem<usize>> {
    let current = current.map(model_reference);
    let mut order: Vec<_> = (0..models.len()).collect();
    order.sort_by_key(|&index| {
        let model = &models[index];
        let reference = Some(model_reference(model));
        (
            reference != current,
            reference != default,
            model.provider.as_str(),
        )
    });
    order
        .into_iter()
        .map(|index| model_item(index, &models[index], current, default))
        .collect()
}

fn model_item(
    index: usize,
    model: &Model,
    current: Option<(&str, &str)>,
    default: Option<(&str, &str)>,
) -> PickerItem<usize> {
    let reference = Some(model_reference(model));
    let marker = if reference == current { "✓ " } else { "  " };
    let default = if reference == default {
        " · default"
    } else {
        ""
    };
    PickerItem {
        label: format!("{marker}{}{default}", model_label(model)),
        search: format!("{}{default}", model_search(model)),
        value: index,
    }
}

pub enum ModelSelection {
    Select(usize),
    SetDefault(usize),
    Cancel,
}

/// The scoped view preserves the configured cycle order. Tab switches scope without losing search.
#[derive(Clone, Copy, Default)]
pub struct ModelPickerOptions<'a> {
    pub scoped: &'a [usize],
    pub current: Option<&'a Model>,
    pub default: Option<(&'a str, &'a str)>,
    pub query: &'a str,
}

pub struct ModelPicker {
    picker: Picker,
    scoped: bool,
    rows: Vec<String>,
    all: Vec<usize>,
    scope: Vec<usize>,
    search: Vec<String>,
    default: Option<usize>,
    current: Option<usize>,
    save: Box<dyn FnMut(usize)>,
}

impl ModelPicker {
    pub fn new(
        models: &[Model],
        options: ModelPickerOptions<'_>,
        theme: Theme,
        select: impl FnMut(ModelSelection) + 'static,
    ) -> Self {
        use std::cell::RefCell;
        use std::rc::Rc;

        let current = options.current.map(model_reference);
        let default = options.default;
        let (mut rows, mut search) = (
            Vec::with_capacity(models.len()),
            Vec::with_capacity(models.len()),
        );
        for (index, model) in models.iter().enumerate() {
            let item = model_item(index, model, current, default);
            rows.push(item.label);
            search.push(item.search);
        }
        let mut all: Vec<_> = (0..models.len()).collect();
        all.sort_by_key(|&index| {
            let model = &models[index];
            let reference = Some(model_reference(model));
            (
                reference != current,
                reference != default,
                model.provider.as_str(),
            )
        });
        let scope: Vec<_> = options
            .scoped
            .iter()
            .copied()
            .filter(|&index| index < models.len())
            .collect();
        let default = models
            .iter()
            .position(|model| Some(model_reference(model)) == default);
        let current = models
            .iter()
            .position(|model| Some(model_reference(model)) == current);
        let scoped = !scope.is_empty();
        let select = Rc::new(RefCell::new(select));
        let confirm = Rc::clone(&select);
        let mut picker = Picker::ranked(
            "Select Model".into(),
            rows.clone(),
            theme,
            model_rank(if scoped { &scope } else { &all }, &search, default),
            move |index| {
                confirm.borrow_mut()(index.map_or(ModelSelection::Cancel, ModelSelection::Select));
            },
        )
        .description(model_scope_description(scoped, !scope.is_empty()))
        .hint("Type to filter · Enter select · Ctrl+S set default · Esc cancel")
        .with_query(options.query);
        if options.query.is_empty()
            && let Some(index) = current
        {
            picker.select_index(index);
        }
        Self {
            picker,
            scoped,
            rows,
            all,
            scope,
            search,
            default,
            current,
            save: Box::new(move |index| select.borrow_mut()(ModelSelection::SetDefault(index))),
        }
    }
}

fn model_scope_description(scoped: bool, has_scope: bool) -> &'static str {
    if scoped {
        "Scope: scoped · Tab to show all configured models"
    } else if has_scope {
        "Scope: all · Tab to show scoped models"
    } else {
        "Only showing models from configured providers. Use /login to add providers."
    }
}

fn model_rank(
    indices: &[usize],
    search: &[String],
    default: Option<usize>,
) -> impl FnMut(&str) -> Vec<usize> + 'static {
    let indices = indices.to_vec();
    let search = search.to_vec();
    move |query| {
        let mut order = pi_rs_tui::fuzzy_filter(&indices, query, |index| &search[*index]);
        let query = query.trim().to_lowercase();
        if let Some(index) = default.filter(|index| indices.contains(index))
            && !query.is_empty()
            && "default".starts_with(&query)
        {
            order.retain(|&candidate| candidate != index);
            order.insert(0, index);
        }
        order
    }
}

impl Component for ModelPicker {
    fn render(&self, width: usize) -> Vec<String> {
        self.picker.render(width)
    }

    fn handle_input(&mut self, data: &str) {
        if self.picker.finished {
            return;
        }
        let keys = get_keybindings();
        if keys.matches(data, "tui.input.tab") && !self.scope.is_empty() {
            self.scoped = !self.scoped;
            let indices = if self.scoped { &self.scope } else { &self.all };
            let keep = self
                .picker
                .query()
                .is_empty()
                .then_some(self.current)
                .flatten();
            self.picker.description = model_scope_description(self.scoped, true).into();
            self.picker.replace_rows(
                self.rows.clone(),
                Box::new(model_rank(indices, &self.search, self.default)),
                keep,
            );
        } else if keys.matches(data, "app.models.save") {
            if let Some(index) = self.picker.selected_index() {
                self.picker.finished = true;
                (self.save)(index);
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

pub fn thinking_description(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "No reasoning",
        ThinkingLevel::Minimal => "Very brief reasoning (~1k tokens)",
        ThinkingLevel::Low => "Light reasoning (~2k tokens)",
        ThinkingLevel::Medium => "Moderate reasoning (~8k tokens)",
        ThinkingLevel::High => "Deep reasoning (~16k tokens)",
        ThinkingLevel::Xhigh => "Extra-high reasoning (~32k tokens)",
        ThinkingLevel::Max => "Maximum reasoning",
    }
}

pub fn thinking_items(
    levels: &[ThinkingLevel],
    current: ThinkingLevel,
    default: Option<ThinkingLevel>,
) -> Vec<PickerItem<ThinkingLevel>> {
    levels
        .iter()
        .map(|&level| {
            let marker = if level == current { "✓ " } else { "  " };
            let default = if Some(level) == default {
                " · default"
            } else {
                ""
            };
            let search = format!(
                "{} · {}{default}",
                level.as_str(),
                thinking_description(level)
            );
            PickerItem {
                label: format!("{marker}{search}"),
                search,
                value: level,
            }
        })
        .collect()
}

pub fn theme_items(names: &[String], current: &str) -> Vec<PickerItem<String>> {
    names
        .iter()
        .map(|name| {
            let label = if name == current {
                format!("{name} (current)")
            } else {
                name.clone()
            };
            PickerItem {
                label,
                search: name.clone(),
                value: name.clone(),
            }
        })
        .collect()
}

pub fn show_images_items() -> Vec<PickerItem<bool>> {
    vec![
        PickerItem::new("Yes · Show images inline in terminal".into(), true),
        PickerItem::new("No · Show text placeholder instead".into(), false),
    ]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthPickerStatus {
    Configured(AuthCheck),
    Unresolved,
}

pub struct AuthPickerProvider<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub method_name: Option<&'a str>,
    pub auth_type: AuthType,
    pub status: Option<&'a AuthPickerStatus>,
    pub subscription: Option<bool>,
}

pub fn auth_provider_type(auth_type: AuthType, subscription: Option<bool>) -> &'static str {
    match auth_type {
        AuthType::ApiKey => "API key",
        AuthType::OAuth if subscription == Some(false) => "account",
        AuthType::OAuth => "subscription",
    }
}

fn is_env_names(source: &str) -> bool {
    source.split(", ").all(|name| {
        let mut bytes = name.bytes();
        bytes.next().is_some_and(|ch| ch.is_ascii_uppercase())
            && bytes.all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == b'_')
    })
}

pub fn auth_provider_items(
    providers: &[AuthPickerProvider<'_>],
) -> Vec<PickerItem<(String, AuthType)>> {
    let mixed = providers.first().is_some_and(|first| {
        providers
            .iter()
            .any(|item| item.auth_type != first.auth_type)
    });
    providers
        .iter()
        .map(|item| {
            let AuthPickerProvider {
                id,
                name,
                method_name,
                auth_type,
                status,
                subscription,
            } = *item;
            let kind = auth_provider_type(auth_type, subscription);
            let mut label = name.to_owned();
            if mixed {
                label.push_str(&format!(" [{kind}]"));
            }
            match status {
                None => label.push_str(" • not configured"),
                Some(AuthPickerStatus::Unresolved) => {
                    label.push_str(" • stored key did not resolve")
                }
                Some(AuthPickerStatus::Configured(status)) if status.auth_type != auth_type => {
                    label.push_str(&format!(
                        " • {} configured",
                        auth_provider_type(status.auth_type, subscription)
                    ))
                }
                Some(AuthPickerStatus::Configured(status)) => match status.source.as_deref() {
                    None | Some("OAuth" | "stored credential") => label.push_str(" ✓ configured"),
                    Some(source) if is_env_names(source) => {
                        label.push_str(&format!(" ✓ env: {source}"))
                    }
                    Some(source) => label.push_str(&format!(" ✓ {source}")),
                },
            }
            PickerItem {
                label,
                search: format!("{name} {id} {kind} {}", method_name.unwrap_or_default()),
                value: (id.to_owned(), auth_type),
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadiusLoginChoice {
    Radius,
    OtherProvider,
    UseExisting,
}

pub fn radius_login_items(has_existing: bool) -> Vec<PickerItem<RadiusLoginChoice>> {
    let mut rows = vec![
        PickerItem::new("Sign in with Radius".into(), RadiusLoginChoice::Radius),
        PickerItem::new("Other providers".into(), RadiusLoginChoice::OtherProvider),
    ];
    if has_existing {
        rows.push(PickerItem::new(
            "Use existing authentication".into(),
            RadiusLoginChoice::UseExisting,
        ));
    }
    rows
}

/// `None` means all available models; an explicit list preserves cycling order and unavailable IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedModels {
    all: Vec<String>,
    enabled: Option<Vec<String>>,
}

impl ScopedModels {
    pub fn new(models: &[Model], enabled: Option<Vec<String>>) -> Self {
        Self {
            all: models
                .iter()
                .map(|model| format!("{}/{}", model.provider, model.id))
                .collect(),
            enabled,
        }
    }

    pub fn enabled(&self) -> Option<&[String]> {
        self.enabled.as_deref()
    }

    pub fn contains(&self, id: &str) -> bool {
        self.enabled
            .as_ref()
            .is_none_or(|list| list.iter().any(|value| value == id))
    }

    pub fn toggle(&mut self, id: &str) {
        let targets = [id];
        self.set_targets(&targets, !self.contains(id));
    }

    pub fn set_targets(&mut self, targets: &[&str], enable: bool) {
        if enable {
            let Some(enabled) = self.enabled.as_mut() else {
                return;
            };
            for id in targets {
                if !enabled.iter().any(|value| value == *id) {
                    enabled.push((*id).to_owned());
                }
            }
            if enabled.len() == self.all.len() && enabled.iter().all(|id| self.all.contains(id)) {
                self.enabled = None;
            }
        } else {
            let enabled = self.enabled.get_or_insert_with(|| self.all.clone());
            enabled.retain(|id| !targets.contains(&id.as_str()));
        }
    }

    pub fn move_by(&mut self, id: &str, delta: isize) -> bool {
        let Some(enabled) = self.enabled.as_mut() else {
            return false;
        };
        let Some(index) = enabled.iter().position(|value| value == id) else {
            return false;
        };
        let Some(target) = index
            .checked_add_signed(delta)
            .filter(|target| *target < enabled.len())
        else {
            return false;
        };
        enabled.swap(index, target);
        true
    }

    fn order(&self) -> Vec<&str> {
        match &self.enabled {
            Some(enabled) => enabled
                .iter()
                .map(String::as_str)
                .chain(
                    self.all
                        .iter()
                        .filter(|id| !enabled.contains(id))
                        .map(String::as_str),
                )
                .collect(),
            None => self.all.iter().map(String::as_str).collect(),
        }
    }
}

pub fn scoped_model_items(models: &[Model], scope: &ScopedModels) -> Vec<PickerItem<String>> {
    scope
        .order()
        .into_iter()
        .map(|id| {
            let model = id.split_once('/').and_then(|reference| {
                models
                    .iter()
                    .find(|model| model_reference(model) == reference)
            });
            let marker = if scope.contains(id) && model.is_some() {
                "✓ "
            } else {
                "  "
            };
            let label = model.map_or_else(|| format!("{id} [unavailable]"), model_label);
            let search = model.map_or_else(
                || id.to_owned(),
                |model| get_model_search_text(&model.id, &model.provider, Some(&model.name)),
            );
            PickerItem {
                label: format!("{marker}{label}"),
                search,
                value: id.to_owned(),
            }
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
pub enum ScopedModelsAction {
    Change(Option<Vec<String>>),
    Persist(Option<Vec<String>>),
    Cancel,
}

pub struct ScopedModelsPicker {
    picker: Picker,
    models: Vec<Model>,
    scope: ScopedModels,
    ids: Vec<String>,
    dirty: bool,
    action: Box<dyn FnMut(ScopedModelsAction)>,
}

impl ScopedModelsPicker {
    pub fn new(
        models: Vec<Model>,
        enabled: Option<Vec<String>>,
        theme: Theme,
        action: impl FnMut(ScopedModelsAction) + 'static,
    ) -> Self {
        let scope = ScopedModels::new(&models, enabled);
        let picker = Picker::new("Model Configuration".into(), Vec::new(), theme, |_| {})
            .max_visible(8)
            .hint("Enter toggle · Ctrl+A all · Ctrl+X clear · Ctrl+P provider · Alt+↑↓ reorder · Ctrl+S save · Esc close");
        let mut dialog = Self {
            picker,
            models,
            scope,
            ids: Vec::new(),
            dirty: false,
            action: Box::new(action),
        };
        dialog.refresh(None);
        dialog
    }

    fn refresh(&mut self, keep: Option<&str>) {
        let rows = scoped_model_items(&self.models, &self.scope);
        let mut search = Vec::with_capacity(rows.len());
        let mut labels = Vec::with_capacity(rows.len());
        self.ids.clear();
        for row in rows {
            labels.push(row.label);
            search.push(row.search);
            self.ids.push(row.value);
        }
        let keep = keep.and_then(|id| self.ids.iter().position(|candidate| candidate == id));
        self.picker
            .replace_rows(labels, Box::new(fuzzy_rank(search)), keep);
        self.picker.description = if self.dirty {
            "Session-only · Unsaved changes"
        } else {
            "Session-only · Ctrl+S saves to settings"
        }
        .into();
    }
}

impl Component for ScopedModelsPicker {
    fn render(&self, width: usize) -> Vec<String> {
        self.picker.render(width)
    }

    fn handle_input(&mut self, data: &str) {
        let keys = get_keybindings();
        let is = |name: &str| keys.matches(data, name);
        let selected = self
            .picker
            .selected_index()
            .and_then(|index| self.ids.get(index));
        let mut changed = false;
        if is("app.models.save") {
            (self.action)(ScopedModelsAction::Persist(self.scope.enabled.clone()));
            self.dirty = false;
            self.picker.description = "Saved to settings".into();
        } else if matches_key_data(data, "ctrl+c") && !self.picker.query().is_empty() {
            self.picker.search.set_value(String::new());
            self.picker.refilter(None);
        } else if is("tui.select.cancel") {
            (self.action)(ScopedModelsAction::Cancel);
        } else if is("tui.select.confirm") || data == "\n" {
            if let Some(id) = selected {
                self.scope.toggle(id);
                changed = true;
            }
        } else if is("app.models.reorderUp") || is("app.models.reorderDown") {
            if let Some(id) = selected {
                changed = self
                    .scope
                    .move_by(id, if is("app.models.reorderUp") { -1 } else { 1 });
            }
        } else if is("app.models.enableAll") || is("app.models.clearAll") {
            let targets: Vec<_> = self
                .picker
                .visible_order()
                .iter()
                .map(|&index| self.ids[index].as_str())
                .collect();
            self.scope.set_targets(&targets, is("app.models.enableAll"));
            changed = true;
        } else if is("app.models.toggleProvider") {
            if let Some((provider, _)) = selected.and_then(|id| id.split_once('/')) {
                let ids: Vec<_> = self
                    .models
                    .iter()
                    .filter(|model| model.provider == provider)
                    .map(|model| format!("{provider}/{}", model.id))
                    .collect();
                if !ids.is_empty() {
                    let enable = ids.iter().any(|id| !self.scope.contains(id));
                    let targets: Vec<_> = ids.iter().map(String::as_str).collect();
                    self.scope.set_targets(&targets, enable);
                    changed = true;
                }
            }
        } else {
            self.picker.handle_input(data);
        }
        if changed {
            let keep = selected.cloned();
            self.dirty = true;
            self.refresh(keep.as_deref());
            (self.action)(ScopedModelsAction::Change(self.scope.enabled.clone()));
        }
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(&mut self.picker)
    }
    fn captures_viewport_input(&self) -> bool {
        true
    }
}
