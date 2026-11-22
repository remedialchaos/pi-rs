//! Model registry: a thin, synchronous facade over `pi_rs_ai::Models` that applies
//! the credential-blind `models.json` snapshot. Ported from `core/model-config.ts`
//! and the configuration side of `core/model-registry.ts`.
//!
//! The upstream class delegates streaming/auth to `ModelRuntime`. This module
//! keeps only the parts that can be exercised offline: loading the file, expanding
//! environment references, composing custom providers, overriding built-in models,
//! and answering `get`/`list`/`refresh` queries. Stream functions remain owned by
//! `pi_rs_ai::Models`.

use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use pi_rs_ai::{
    Api, BaseModel, Model, ModelCost, ModelCostRates, ModelInputLimits, ModelInputModality,
    ModelPromptCache, Models, ModelsOptions, Provider, SamplingParams,
    SamplingParamsByThinkingLevel, ThinkingLevel,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// File name looked up under the agent directory.
pub const MODELS_JSON_FILE: &str = "models.json";

/// One immutable load of `models.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsConfig {
    #[serde(rename = "$schema", skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(default)]
    pub providers: IndexMap<String, ModelsJsonProvider>,
}

/// A provider entry in `models.json`. A provider with `models` but no `api` or
/// `baseUrl` is invalid; a provider with only `baseUrl`/`headers`/`compat` is an
/// override of a built-in provider and keeps that provider's model list.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsJsonProvider {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<IndexMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compat: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_header: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelConfigFields>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub model_overrides: IndexMap<String, ModelConfigFields>,
}

/// Fields shared by `models[]` entries and `modelOverrides` entries. `id`, `api`
/// and `baseUrl` are required only when defining a new model and are ignored in
/// an override entry. Numeric limits are optional so a partial override can leave
/// the catalog value intact.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelConfigFields {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<IndexMap<ThinkingLevel, Option<String>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Vec<ModelInputModality>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_limits: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache: Option<ModelPromptCache>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampling_params: Option<SamplingParams>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampling_params_by_thinking_level: Option<SamplingParamsByThinkingLevel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<IndexMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compat: Option<Map<String, Value>>,
}

pub type ModelsJsonModel = ModelConfigFields;
pub type ModelsJsonModelOverride = ModelConfigFields;

/// Remove a leading UTF-8 byte order mark from decoded text.
pub fn strip_bom(content: &str) -> &str {
    content.strip_prefix('\u{feff}').unwrap_or(content)
}

/// Strip `//` and `/* ... */` comments without touching comment markers inside
/// JSON string values (in particular the `//` in `https://` base URLs).
pub fn strip_json_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(ch) = chars.next() {
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => {
                in_string = true;
                out.push(ch);
            }
            '/' if chars.peek() == Some(&'/') => {
                chars.next();
                for next in chars.by_ref() {
                    if next == '\n' || next == '\r' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut previous = '\0';
                for next in chars.by_ref() {
                    if previous == '*' && next == '/' {
                        break;
                    }
                    previous = next;
                }
            }
            _ => out.push(ch),
        }
    }
    out
}

fn is_name_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn env_value(name: &str, env: &IndexMap<String, String>) -> Option<String> {
    if let Some(value) = env.get(name).filter(|value| !value.is_empty()) {
        return Some(value.clone());
    }
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// Resolve a config value the way upstream `resolveConfigValue` does for
/// environment references: `$NAME` and `${NAME}` are interpolated, `$$` and `$!`
/// escape a literal `$`/`!`, and a literal is returned unchanged. `!command`
/// values require a shell and are treated as unresolved here; the registry keeps
/// the raw value rather than executing a process.
pub fn resolve_config_value(value: &str, env: &IndexMap<String, String>) -> Option<String> {
    if value.starts_with('!') {
        return None;
    }
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte != b'$' {
            let ch = value[index..].chars().next()?;
            out.push(ch);
            index += ch.len_utf8();
            continue;
        }
        match bytes.get(index + 1).copied() {
            Some(b'$') => {
                out.push('$');
                index += 2;
            }
            Some(b'!') => {
                out.push('!');
                index += 2;
            }
            Some(b'{') => {
                let Some(end) = value[index + 2..].find('}') else {
                    out.push('$');
                    index += 1;
                    continue;
                };
                let end = index + 2 + end;
                let name = &value[index + 2..end];
                if !name.is_empty()
                    && is_name_start(name.as_bytes()[0])
                    && name.bytes().all(is_name_byte)
                {
                    out.push_str(&env_value(name, env)?);
                } else {
                    out.push_str(&value[index..=end]);
                }
                index = end + 1;
            }
            Some(next) if is_name_start(next) => {
                let mut end = index + 1;
                while end < bytes.len() && is_name_byte(bytes[end]) {
                    end += 1;
                }
                out.push_str(&env_value(&value[index + 1..end], env)?);
                index = end;
            }
            _ => {
                out.push('$');
                index += 1;
            }
        }
    }
    Some(out)
}

/// Expand a value, falling back to the original literal when it cannot be
/// resolved. Used for `baseUrl` and `apiKey`, which upstream keeps verbatim in
/// that case so the request layer can report a useful error.
pub fn expand_or_literal(value: &str, env: &IndexMap<String, String>) -> String {
    resolve_config_value(value, env).unwrap_or_else(|| value.to_string())
}

/// Expand header values, dropping entries whose referenced environment variable
/// is missing (upstream `resolveHeaders`).
pub fn expand_headers(
    headers: &IndexMap<String, String>,
    env: &IndexMap<String, String>,
) -> IndexMap<String, String> {
    let mut out = IndexMap::new();
    for (name, value) in headers {
        if let Some(resolved) = resolve_config_value(value, env) {
            out.insert(name.clone(), resolved);
        }
    }
    out
}

/// Parse a decoded `models.json` document. Missing files are handled by
/// [`load_models_config`]; this function reports parse errors with the document
/// text already BOM/comment stripped.
pub fn parse_models_config(content: &str) -> Result<ModelsConfig, String> {
    let text = strip_json_comments(strip_bom(content));
    serde_json::from_str(&text).map_err(|error| format!("Failed to parse models.json: {error}"))
}

/// Load a `models.json` snapshot. A missing file produces an empty config and no
/// error, matching upstream. Read failures and parse failures are reported.
pub fn load_models_config(path: &Path) -> Result<ModelsConfig, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ModelsConfig::default());
        }
        Err(error) => {
            return Err(format!(
                "Failed to load models.json: {error}\n\nFile: {}",
                path.display()
            ));
        }
    };
    let text = String::from_utf8_lossy(&bytes);
    parse_models_config(&text).map_err(|error| format!("{error}\n\nFile: {}", path.display()))
}

fn deep_merge(base: &mut Map<String, Value>, patch: &Map<String, Value>) {
    for (key, value) in patch {
        if let (Some(target), Some(source)) = (
            base.get_mut(key).and_then(Value::as_object_mut),
            value.as_object(),
        ) {
            deep_merge(target, source);
        } else {
            base.insert(key.clone(), value.clone());
        }
    }
}

fn merge_cost(cost: &mut ModelCost, patch: &Map<String, Value>) {
    let rates = &mut cost.rates;
    if let Some(value) = patch.get("input").and_then(Value::as_f64) {
        rates.input = value;
    }
    if let Some(value) = patch.get("output").and_then(Value::as_f64) {
        rates.output = value;
    }
    if let Some(value) = patch
        .get("cacheRead")
        .or_else(|| patch.get("cache_read"))
        .and_then(Value::as_f64)
    {
        rates.cache_read = value;
    }
    if let Some(value) = patch
        .get("cacheWrite")
        .or_else(|| patch.get("cache_write"))
        .and_then(Value::as_f64)
    {
        rates.cache_write = value;
    }
}

fn merge_headers(target: &mut Option<IndexMap<String, String>>, patch: &IndexMap<String, String>) {
    let headers = target.get_or_insert_with(IndexMap::new);
    for (name, value) in patch {
        headers.insert(name.clone(), value.clone());
    }
}

fn numeric_u64(value: Option<f64>) -> Option<u64> {
    value.and_then(|number| {
        if number.is_finite() && number > 0.0 {
            Some(number as u64)
        } else {
            None
        }
    })
}

fn apply_model_fields(
    model: &mut Model,
    fields: &ModelConfigFields,
    env: &IndexMap<String, String>,
) -> Result<(), String> {
    if let Some(name) = &fields.name {
        model.name = name.clone();
    }
    if let Some(reasoning) = fields.reasoning {
        model.reasoning = reasoning;
    }
    if let Some(input) = &fields.input {
        model.input = input.clone();
    }
    if let Some(value) = numeric_u64(fields.context_window) {
        model.context_window = value;
    }
    if let Some(value) = numeric_u64(fields.max_tokens) {
        model.max_tokens = value;
    }
    if let Some(value) = &fields.prompt_cache {
        model.prompt_cache = Some(*value);
    }
    if let Some(value) = &fields.thinking_level_map {
        model.thinking_level_map = Some(value.clone());
    }
    if let Some(value) = &fields.sampling_params {
        model.sampling_params = Some(value.clone());
    }
    if let Some(value) = &fields.sampling_params_by_thinking_level {
        model.sampling_params_by_thinking_level = Some(value.clone());
    }
    if let Some(value) = &fields.input_limits {
        let merged =
            serde_json::to_value(model.input_limits.as_ref()).map_err(|error| error.to_string())?;
        let mut merged = merged.as_object().cloned().unwrap_or_default();
        if let Some(patch) = value.as_object() {
            deep_merge(&mut merged, patch);
        }
        model.input_limits =
            serde_json::from_value(Value::Object(merged)).map_err(|error| error.to_string())?;
    }
    if let Some(patch) = &fields.cost {
        merge_cost(&mut model.cost, patch);
    }
    if let Some(patch) = &fields.compat {
        let compat = model.compat.get_or_insert_with(Map::new);
        deep_merge(compat, patch);
    }
    if let Some(headers) = &fields.headers {
        merge_headers(&mut model.headers, &expand_headers(headers, env));
    }
    Ok(())
}

fn build_model(
    provider_id: &str,
    fields: &ModelConfigFields,
    env: &IndexMap<String, String>,
) -> Result<Model, String> {
    let id = fields
        .id
        .as_deref()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| format!("Provider {provider_id}, model: no \"id\" specified."))?
        .to_string();
    let api_text = fields
        .api
        .as_deref()
        .ok_or_else(|| format!("Provider {provider_id}, model {id}: no \"api\" specified."))?;
    let base_url = fields
        .base_url
        .as_deref()
        .filter(|url| !url.is_empty())
        .ok_or_else(|| {
            format!("Provider {provider_id}: \"baseUrl\" is required when defining custom models.")
        })?;
    let mut model = Model {
        base: BaseModel {
            id,
            name: String::new(),
            api: Api::from(api_text),
            provider: provider_id.to_string(),
            base_url: expand_or_literal(base_url, env),
            input: fields
                .input
                .clone()
                .unwrap_or_else(|| vec![ModelInputModality::Text]),
            input_limits: match &fields.input_limits {
                Some(value) => Some(
                    serde_json::from_value::<ModelInputLimits>(value.clone())
                        .map_err(|error| error.to_string())?,
                ),
                None => None,
            },
            cost: ModelCost {
                rates: ModelCostRates::default(),
                tiers: Vec::new(),
            },
            headers: None,
        },
        kind: None,
        reasoning: fields.reasoning.unwrap_or(false),
        thinking_level_map: fields.thinking_level_map.clone(),
        prompt_cache: fields.prompt_cache,
        context_window: numeric_u64(fields.context_window).unwrap_or(128_000),
        max_tokens: numeric_u64(fields.max_tokens).unwrap_or(16_384),
        sampling_params: fields.sampling_params.clone(),
        sampling_params_by_thinking_level: fields.sampling_params_by_thinking_level.clone(),
        compat: fields.compat.clone(),
    };
    let name = fields.name.clone().unwrap_or_else(|| model.id.clone());
    model.name = name;
    if let Some(cost) = &fields.cost {
        merge_cost(&mut model.cost, cost);
    }
    if let Some(headers) = &fields.headers {
        merge_headers(&mut model.headers, &expand_headers(headers, env));
    }
    Ok(model)
}

fn apply_provider_config(
    provider: &mut Provider,
    config: &ModelsJsonProvider,
    env: &IndexMap<String, String>,
) -> Result<(), String> {
    let id = provider.id.clone();
    if let Some(name) = &config.name {
        provider.name = name.clone();
    }
    if let Some(base_url) = &config.base_url {
        provider.base_url = Some(expand_or_literal(base_url, env));
    }
    if let Some(headers) = &config.headers {
        let resolved = expand_headers(headers, env);
        let target = provider.headers.get_or_insert_with(IndexMap::new);
        for (name, value) in resolved {
            target.insert(name, value);
        }
    }

    let mut chat: Vec<Model> = provider
        .models
        .iter()
        .filter_map(|model| match model {
            pi_rs_ai::AnyModel::Chat(model) => Some(model.clone()),
            _ => None,
        })
        .collect();
    let other: Vec<pi_rs_ai::AnyModel> = provider
        .models
        .iter()
        .filter(|model| !matches!(model, pi_rs_ai::AnyModel::Chat(_)))
        .cloned()
        .collect();

    let provider_base_url = provider.base_url.clone().unwrap_or_default();
    let provider_headers = provider.headers.clone().unwrap_or_default();
    for model in &mut chat {
        if let Some(api) = &config.api {
            model.api = Api::from(api.as_str());
        }
        if !provider_base_url.is_empty() {
            model.base_url = provider_base_url.clone();
        }
        if !provider_headers.is_empty() {
            merge_headers(&mut model.headers, &provider_headers);
        }
        if let Some(compat) = &config.compat {
            let target = model.compat.get_or_insert_with(Map::new);
            deep_merge(target, compat);
        }
    }

    for definition in &config.models {
        // A model without its own api/baseUrl inherits the provider-level values.
        let mut effective = definition.clone();
        if effective.api.is_none() {
            effective.api = config.api.clone();
        }
        if effective.base_url.is_none() && !provider_base_url.is_empty() {
            effective.base_url = Some(provider_base_url.clone());
        }
        let mut model = build_model(&id, &effective, env)?;
        if !provider_headers.is_empty() {
            merge_headers(&mut model.headers, &provider_headers);
        }
        if let Some(compat) = &config.compat {
            let target = model.compat.get_or_insert_with(Map::new);
            deep_merge(target, compat);
            if let Some(own) = &definition.compat {
                deep_merge(target, own);
            }
        }
        if let Some(position) = chat.iter().position(|existing| existing.id == model.id) {
            chat[position] = model;
        } else {
            chat.push(model);
        }
    }

    for (model_id, override_fields) in &config.model_overrides {
        if let Some(model) = chat.iter_mut().find(|model| model.id == *model_id) {
            apply_model_fields(model, override_fields, env)?;
        }
    }

    let mut models = other;
    models.extend(chat.into_iter().map(pi_rs_ai::AnyModel::Chat));
    provider.models = models.into();
    Ok(())
}

/// Apply a configuration snapshot to `models`, returning the provider ids that
/// were installed. Existing ids are replaced wholesale; ids removed from the
/// configuration are deleted by [`ModelRegistry::refresh`].
pub fn apply_config(
    models: &Models,
    config: &ModelsConfig,
    env: &IndexMap<String, String>,
) -> Result<Vec<String>, String> {
    let mut applied = Vec::new();
    for (id, config) in &config.providers {
        let mut provider = models.get_provider(id).unwrap_or_else(|| {
            let name = config.name.clone().unwrap_or_else(|| id.clone());
            Provider::new(id.clone(), name)
        });
        if provider.models.is_empty() && config.models.is_empty() && config.api.is_none() {
            return Err(format!(
                "Provider {id}: must specify \"baseUrl\", \"headers\", \"compat\", \"modelOverrides\", or \"models\"."
            ));
        }
        apply_provider_config(&mut provider, config, env)
            .map_err(|error| format!("Provider {id}: {error}"))?;
        models.set_provider(provider);
        applied.push(id.clone());
    }
    Ok(applied)
}

/// Synchronous registry facade holding the last applied configuration.
pub struct ModelRegistry {
    models: Models,
    path: Option<PathBuf>,
    config: ModelsConfig,
    error: Option<String>,
    applied: Vec<String>,
    /// Explicit environment overlay for `$VAR` interpolation. Process environment
    /// variables are still consulted as a fallback.
    env: IndexMap<String, String>,
    /// Provider snapshots taken before any `models.json` overrides were applied.
    pristine: IndexMap<String, Provider>,
}

impl std::fmt::Debug for ModelRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModelRegistry")
            .field("path", &self.path)
            .field("providers", &self.config.providers.len())
            .field("error", &self.error)
            .finish()
    }
}

impl ModelRegistry {
    /// Build a registry over an existing `Models` instance.
    pub fn new(models: Models, path: Option<PathBuf>) -> Self {
        let pristine: IndexMap<String, Provider> = models
            .get_providers()
            .into_iter()
            .map(|provider| (provider.id.clone(), provider))
            .collect();
        let mut registry = Self {
            models,
            path,
            config: ModelsConfig::default(),
            error: None,
            applied: Vec::new(),
            env: IndexMap::new(),
            pristine,
        };
        registry.reload();
        registry
    }

    /// Override the environment used to expand `$VAR` references and reapply
    /// the configuration. Process variables remain available as a fallback.
    pub fn with_env(mut self, env: IndexMap<String, String>) -> Self {
        self.env = env;
        self.reload();
        self
    }

    /// Build a registry seeded with the embedded built-in provider catalog.
    pub fn with_builtin(options: ModelsOptions, path: Option<PathBuf>) -> Result<Self, String> {
        let models = Models::builtin(options).map_err(|error| error.to_string())?;
        Ok(Self::new(models, path))
    }

    /// The underlying model store, for callers that need streaming/auth.
    pub fn models(&self) -> &Models {
        &self.models
    }

    /// Last configuration load/apply error, if any.
    pub fn get_error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The last loaded configuration snapshot.
    pub fn config(&self) -> &ModelsConfig {
        &self.config
    }

    /// Path of the `models.json` file, when configured.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn reload(&mut self) {
        let (config, error) = match &self.path {
            None => (ModelsConfig::default(), None),
            Some(path) => match load_models_config(path) {
                Ok(config) => (config, None),
                Err(error) => (ModelsConfig::default(), Some(error)),
            },
        };
        // Rebuild from the pristine catalog so removed custom models and
        // overrides do not survive a refresh.
        for provider in self.pristine.values() {
            self.models.set_provider(provider.clone());
        }
        for id in self.applied.iter().chain(config.providers.keys()) {
            if !self.pristine.contains_key(id) {
                self.models.delete_provider(id);
            }
        }
        let applied = match apply_config(&self.models, &config, &self.env) {
            Ok(applied) => applied,
            Err(apply_error) => {
                self.error = Some(match error {
                    Some(load_error) => format!("{load_error}\n{apply_error}"),
                    None => apply_error,
                });
                self.config = config;
                return;
            }
        };
        self.applied = applied;
        self.config = config;
        self.error = error;
    }

    /// Reload `models.json` from disk and reapply all overrides.
    pub fn refresh(&mut self) -> Result<(), String> {
        self.reload();
        self.error.clone().map_or(Ok(()), Err)
    }

    /// Find one chat model by provider and id.
    pub fn get(&self, provider: &str, model_id: &str) -> Option<Model> {
        self.models.get_model(provider, model_id)
    }

    /// All known chat models in provider/catalog order.
    pub fn list(&self) -> Vec<Model> {
        self.models.get_models(None)
    }

    /// Chat models belonging to one provider.
    pub fn list_for_provider(&self, provider: &str) -> Vec<Model> {
        self.models.get_models(Some(provider))
    }

    /// Provider metadata, including dynamically configured providers.
    pub fn get_provider(&self, provider: &str) -> Option<Provider> {
        self.models.get_provider(provider)
    }

    /// Provider ids in insertion order.
    pub fn provider_ids(&self) -> Vec<String> {
        self.models
            .get_providers()
            .into_iter()
            .map(|provider| provider.id)
            .collect()
    }

    /// Display name from the configured provider, falling back to its id.
    pub fn provider_display_name(&self, provider: &str) -> String {
        self.models
            .get_provider(provider)
            .map_or_else(|| provider.to_string(), |entry| entry.name)
    }

    /// Resolved `apiKey` for a provider from `models.json`, if configured.
    pub fn api_key_for_provider(&self, provider: &str) -> Option<String> {
        self.config
            .providers
            .get(provider)
            .and_then(|config| config.api_key.as_deref())
            .and_then(|key| resolve_config_value(key, &self.env))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(dir: &std::path::Path, text: &str) -> PathBuf {
        let path = dir.join(MODELS_JSON_FILE);
        if let Err(error) = std::fs::write(&path, text) {
            panic!("failed to write fixture: {error}");
        }
        path
    }

    fn registry_with(path: &Path) -> ModelRegistry {
        let models = Models::default();
        models.set_provider(Provider::new("anthropic", "Anthropic").with_models(vec![
            pi_rs_ai::AnyModel::Chat(Model {
                base: BaseModel {
                    id: "claude-test".into(),
                    name: "Claude Test".into(),
                    api: Api::AnthropicMessages,
                    provider: "anthropic".into(),
                    base_url: "https://api.anthropic.com".into(),
                    input: vec![ModelInputModality::Text],
                    input_limits: None,
                    cost: ModelCost {
                        rates: ModelCostRates {
                            input: 1.0,
                            output: 2.0,
                            cache_read: 0.1,
                            cache_write: 3.0,
                        },
                        tiers: Vec::new(),
                    },
                    headers: None,
                },
                kind: None,
                reasoning: true,
                thinking_level_map: None,
                prompt_cache: None,
                context_window: 100_000,
                max_tokens: 8_192,
                sampling_params: None,
                sampling_params_by_thinking_level: None,
                compat: None,
            }),
        ]));
        ModelRegistry::new(models, Some(path.to_path_buf()))
    }

    #[test]
    fn strip_bom_removes_leading_marker_only() -> Result<(), String> {
        assert_eq!(strip_bom("\u{feff}{}"), "{}");
        assert_eq!(strip_bom("a\u{feff}b"), "a\u{feff}b");
        Ok(())
    }

    #[test]
    fn strip_json_comments_keeps_urls_inside_strings() {
        let input =
            "{\n // note\n \"baseUrl\": \"https://example.com/v1\", /* block */ \"id\": \"a\"\n}";
        let stripped = strip_json_comments(input);
        assert!(!stripped.contains("// note"));
        assert!(stripped.contains("https://example.com/v1"));
        assert!(parse_models_config(&stripped).is_ok());
    }

    #[test]
    fn missing_file_is_an_empty_config() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let config = load_models_config(&dir.path().join("absent.json"))?;
        assert!(config.providers.is_empty());
        Ok(())
    }

    #[test]
    fn invalid_json_reports_parse_error() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let path = write_config(dir.path(), "{ not json");
        let error = load_models_config(&path).expect_err("invalid JSON must fail");
        assert!(error.contains("Failed to parse models.json"));
        assert!(error.contains("models.json"));
        Ok(())
    }

    #[test]
    fn env_expansion_handles_named_and_braced_references() {
        let mut env = IndexMap::new();
        env.insert("TOKEN".into(), "secret".into());
        assert_eq!(
            resolve_config_value("$TOKEN", &env).as_deref(),
            Some("secret")
        );
        assert_eq!(
            resolve_config_value("${TOKEN}/v1", &env).as_deref(),
            Some("secret/v1")
        );
        assert_eq!(
            resolve_config_value("$$TOKEN", &env).as_deref(),
            Some("$TOKEN")
        );
        assert_eq!(resolve_config_value("$!", &env).as_deref(), Some("!"));
    }

    #[test]
    fn env_expansion_fails_on_missing_reference() {
        let env = IndexMap::new();
        assert_eq!(resolve_config_value("$MISSING_XYZ", &env), None);
        assert_eq!(resolve_config_value("!run-command", &env), None);
        assert_eq!(
            resolve_config_value("literal-key", &env).as_deref(),
            Some("literal-key")
        );
    }

    #[test]
    fn expand_headers_drops_unresolvable_values() {
        let mut env = IndexMap::new();
        env.insert("ACCOUNT".into(), "acct-1".into());
        let mut headers = IndexMap::new();
        headers.insert("x-account".into(), "$ACCOUNT".into());
        headers.insert("x-missing".into(), "$NOPE".into());
        let expanded = expand_headers(&headers, &env);
        assert_eq!(
            expanded.get("x-account").map(String::as_str),
            Some("acct-1")
        );
        assert_eq!(expanded.get("x-missing"), None);
    }

    #[test]
    fn custom_provider_models_are_listed_and_findable() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        write_config(
            dir.path(),
            r#"{"providers":{"demo":{"baseUrl":"https://demo.test/v1","apiKey":"key","api":"openai-completions","models":[{"id":"demo-1","reasoning":true,"input":["text"]}]}}}"#,
        );
        let registry = registry_with(&dir.path().join(MODELS_JSON_FILE));
        assert_eq!(registry.get_error(), None);
        assert!(
            registry
                .list()
                .iter()
                .any(|m| m.id == "demo-1" && m.provider == "demo")
        );
        let model = registry
            .get("demo", "demo-1")
            .ok_or("missing custom model")?;
        assert_eq!(model.name, "demo-1");
        assert_eq!(model.base_url, "https://demo.test/v1");
        assert!(model.reasoning);
        Ok(())
    }

    #[test]
    fn custom_provider_needs_api_and_base_url() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        write_config(
            dir.path(),
            r#"{"providers":{"broken":{"api":"openai-completions","models":[{"id":"m"}]}}}"#,
        );
        let registry = registry_with(&dir.path().join(MODELS_JSON_FILE));
        let error = registry.get_error().unwrap_or("");
        assert!(error.contains("baseUrl"), "error was: {error}");
        Ok(())
    }

    #[test]
    fn base_url_override_keeps_builtin_models() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        write_config(
            dir.path(),
            r#"{"providers":{"anthropic":{"baseUrl":"https://proxy.test/v1"}}}"#,
        );
        let registry = registry_with(&dir.path().join(MODELS_JSON_FILE));
        let models = registry.list_for_provider("anthropic");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].base_url, "https://proxy.test/v1");
        Ok(())
    }

    #[test]
    fn model_override_renames_and_merges_compat() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        write_config(
            dir.path(),
            r#"{"providers":{"anthropic":{"compat":{"supportsUsageInStreaming":false},"modelOverrides":{"claude-test":{"name":"Renamed","compat":{"supportsStrictMode":true},"cost":{"input":9}}}}}}"#,
        );
        let registry = registry_with(&dir.path().join(MODELS_JSON_FILE));
        let model = registry
            .get("anthropic", "claude-test")
            .ok_or("missing model")?;
        assert_eq!(model.name, "Renamed");
        assert_eq!(model.cost.rates.input, 9.0);
        assert_eq!(model.cost.rates.output, 2.0);
        let compat = model.compat.ok_or("missing compat")?;
        assert_eq!(
            compat.get("supportsUsageInStreaming"),
            Some(&Value::Bool(false))
        );
        assert_eq!(compat.get("supportsStrictMode"), Some(&Value::Bool(true)));
        Ok(())
    }

    #[test]
    fn refresh_picks_up_new_configuration() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let path = write_config(
            dir.path(),
            r#"{"providers":{"demo":{"baseUrl":"https://one.test","api":"openai-completions","models":[{"id":"one"}]}}}"#,
        );
        let mut registry = registry_with(&path);
        assert!(registry.get("demo", "one").is_some());
        write_config(
            dir.path(),
            r#"{"providers":{"demo":{"baseUrl":"https://two.test","api":"openai-completions","models":[{"id":"two"}]}}}"#,
        );
        registry.refresh()?;
        assert!(registry.get("demo", "one").is_none());
        let model = registry
            .get("demo", "two")
            .ok_or("missing refreshed model")?;
        assert_eq!(model.base_url, "https://two.test");
        Ok(())
    }

    #[test]
    fn refresh_removes_provider_absent_from_config() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let path = write_config(
            dir.path(),
            r#"{"providers":{"extra":{"baseUrl":"https://x.test","api":"openai-completions","models":[{"id":"x"}]}}}"#,
        );
        let mut registry = registry_with(&path);
        assert!(registry.get("extra", "x").is_some());
        write_config(dir.path(), "{}");
        registry.refresh()?;
        assert!(registry.get_provider("extra").is_none());
        Ok(())
    }

    #[test]
    fn env_value_expands_base_url_and_headers() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        write_config(
            dir.path(),
            r#"{"providers":{"envdemo":{"baseUrl":"$PI_TEST_BASE_URL","headers":{"x-account":"$PI_TEST_BASE_URL"},"api":"openai-completions","models":[{"id":"env-model"}]}}}"#,
        );
        let mut env = IndexMap::new();
        env.insert("PI_TEST_BASE_URL".into(), "https://env.test/v1".into());
        let registry =
            ModelRegistry::new(Models::default(), Some(dir.path().join(MODELS_JSON_FILE)))
                .with_env(env);
        let model = registry
            .get("envdemo", "env-model")
            .ok_or("missing model")?;
        assert_eq!(model.base_url, "https://env.test/v1");
        assert_eq!(
            model
                .headers
                .as_ref()
                .and_then(|headers| headers.get("x-account"))
                .map(String::as_str),
            Some("https://env.test/v1")
        );
        Ok(())
    }
}
