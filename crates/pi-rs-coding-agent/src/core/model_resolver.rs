//! Model reference resolution, scoped patterns and initial selection. Ported from
//! `core/model-resolver.ts` (with `isValidThinkingLevel` from `cli/args.ts`).
//!
//! Candidates are collected before a provider is chosen so catalog order never
//! decides the provider; an ambiguous match is reported instead of falling
//! through to a weaker one.

use globset::GlobBuilder;
use pi_rs_ai::{Model, ThinkingLevel};

/// Default thinking level used when neither a scope nor settings specify one.
pub const DEFAULT_THINKING_LEVEL: ThinkingLevel = ThinkingLevel::Medium;

/// Default chat model ids for providers with built-in chat models.
pub const DEFAULT_MODEL_PER_PROVIDER: &[(&str, &str)] = &[
    ("amazon-bedrock", "us.anthropic.claude-opus-5-5"),
    ("ant-ling", "Ring-2.6-1T"),
    ("anthropic", "claude-opus-5-5"),
    ("openai", "gpt-6.2-sol"),
    ("azure", "gpt-6.2-sol"),
    ("openai-codex", "gpt-6.2-sol"),
    ("radius", "balanced"),
    ("nvidia", "nvidia/nemotron-3.5-super-vl-120b-a12b"),
    ("deepseek", "deepseek-v4-pro"),
    ("google", "gemini-4-argon"),
    ("google-vertex", "gemini-4-argon"),
    ("github-copilot", "gpt-6.1-sol"),
    ("openrouter", "moonshotai/kimi-k2.7-code"),
    ("vercel-ai-gateway", "zai/glm-5.3"),
    ("xai", "grok-4.7"),
    ("groq", "openai/gpt-oss-120b"),
    ("cerebras", "gpt-oss-120b"),
    ("zai", "glm-5.3"),
    ("zai-coding-cn", "glm-5.3"),
    ("mistral", "mistral-medium-latest"),
    ("minimax", "MiniMax-M3"),
    ("minimax-cn", "MiniMax-M3"),
    ("moonshotai", "kimi-k2.7-code"),
    ("moonshotai-cn", "kimi-k2.7-code"),
    ("huggingface", "moonshotai/Kimi-K2.7-Code"),
    ("fireworks", "accounts/fireworks/models/kimi-k3"),
    ("together", "moonshotai/Kimi-K3"),
    ("baseten", "zai-org/GLM-5.3"),
    ("opencode", "kimi-k2.7-code"),
    ("opencode-go", "kimi-k3.1-pro"),
    ("kimi-coding", "kimi-for-coding"),
    ("meta", "muse-spark-1.3"),
    ("cloudflare-workers-ai", "@cf/moonshotai/kimi-k2.7-code"),
    (
        "cloudflare-ai-gateway",
        "workers-ai/@cf/moonshotai/kimi-k2.7-code",
    ),
    ("qwen-token-plan", "qwen3.8-max"),
    ("qwen-token-plan-cn", "qwen3.8-max"),
    ("qwen-token-plan-individual", "qwen3.8-max"),
    ("qwen-token-plan-individual-cn", "qwen3.8-max"),
    ("xiaomi", "mimo-v2.6-pro"),
    ("xiaomi-token-plan-cn", "mimo-v2.6-pro"),
    ("xiaomi-token-plan-ams", "mimo-v2.6-pro"),
    ("xiaomi-token-plan-sgp", "mimo-v2.6-pro"),
];

/// Look up the documented default model id for a provider.
pub fn default_model_per_provider(provider: &str) -> Option<&'static str> {
    DEFAULT_MODEL_PER_PROVIDER
        .iter()
        .find(|(id, _)| *id == provider)
        .map(|(_, model)| *model)
}

/// Parse a thinking level name. Only the seven upstream levels are valid.
pub fn parse_thinking_level(value: &str) -> Option<ThinkingLevel> {
    ThinkingLevel::ALL
        .into_iter()
        .find(|level| level.as_str() == value)
}

/// A model plus an explicitly requested thinking level.
#[derive(Clone, Debug, PartialEq)]
pub struct ScopedModel {
    pub model: Model,
    pub thinking_level: Option<ThinkingLevel>,
}

/// How a trailing `:<suffix>` is interpreted.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ThinkingSuffix {
    /// The suffix stays part of the model id.
    None,
    /// A valid thinking level is parsed; any other suffix stays part of the id.
    #[default]
    Strict,
    /// Like strict, but an invalid suffix is dropped with a warning.
    Lenient,
}

/// A bare model id or `provider/id`, optionally followed by `:<thinking>`.
#[derive(Clone, Debug)]
pub struct ModelReferenceRequest<'a> {
    pub text: &'a str,
    pub provider: Option<&'a str>,
    pub thinking_suffix: ThinkingSuffix,
    pub fuzzy: bool,
}

impl<'a> ModelReferenceRequest<'a> {
    pub fn new(text: &'a str) -> Self {
        Self {
            text,
            provider: None,
            thinking_suffix: ThinkingSuffix::Strict,
            fuzzy: true,
        }
    }
}

/// When no existing model matched, the information needed to construct one.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelReferenceTarget {
    pub provider: String,
    pub provider_models: Vec<Model>,
    /// Text after the provider prefix as typed, including any `:<suffix>`.
    pub model_text: String,
    /// `model_text` without a valid thinking suffix (unless suffix mode is none).
    pub model_id: String,
    pub thinking_level: Option<ThinkingLevel>,
}

#[expect(
    clippy::large_enum_variant,
    reason = "Resolved and Ambiguous both carry full Model lists; boxing them would force clones on every lookup."
)]
#[derive(Clone, Debug, PartialEq)]
pub enum ModelReferenceResult {
    Resolved {
        model: Model,
        thinking_level: Option<ThinkingLevel>,
        warning: Option<String>,
    },
    Ambiguous {
        candidates: Vec<Model>,
        usable_count: usize,
        thinking_level: Option<ThinkingLevel>,
        warning: Option<String>,
    },
    NotFound {
        target: Option<ModelReferenceTarget>,
    },
}

impl ModelReferenceResult {
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound { .. })
    }
}

type ResolvedOrAmbiguous = ModelReferenceResult;

struct ThinkingSplit {
    base: String,
    thinking_level: ThinkingLevel,
}

fn split_thinking_suffix(text: &str) -> Option<ThinkingSplit> {
    let colon = text.rfind(':')?;
    let thinking_level = parse_thinking_level(&text[colon + 1..])?;
    Some(ThinkingSplit {
        base: text[..colon].to_string(),
        thinking_level,
    })
}

fn split_provider_prefix(text: &str, models: &[Model]) -> Option<(String, String)> {
    // Provider ids may themselves contain "/" (e.g. URL-derived ids), so match
    // known ids longest first instead of splitting on the first slash.
    let lower_text = text.to_lowercase();
    let mut best: Option<(String, String)> = None;
    for model in models {
        let prefix = model.provider.trim().to_lowercase();
        if prefix.is_empty()
            || best
                .as_ref()
                .is_some_and(|(id, _)| id.len() >= prefix.len())
        {
            continue;
        }
        let Some(tail) = lower_text.get(prefix.len()..) else {
            continue;
        };
        let Some(offset) = tail.find('/') else {
            continue;
        };
        let slash = prefix.len() + offset;
        if lower_text[..slash].trim() != prefix {
            continue;
        }
        let model_text = text[slash + 1..].trim().to_string();
        if !model_text.is_empty() {
            best = Some((model.provider.clone(), model_text));
        }
    }
    best
}

/// Resolve a user-typed model reference against a catalog.
pub fn resolve_model_reference(
    request: &ModelReferenceRequest<'_>,
    models: &[Model],
    usable: &dyn Fn(&str) -> bool,
) -> ModelReferenceResult {
    let text = request.text.trim();
    if text.is_empty() {
        return ModelReferenceResult::NotFound { target: None };
    }
    let constraint = request.provider.map(str::to_lowercase);
    let pool: Vec<Model> = match &constraint {
        Some(constraint) => models
            .iter()
            .filter(|model| model.provider.to_lowercase() == *constraint)
            .cloned()
            .collect(),
        None => models.to_vec(),
    };
    let result = resolve_text(text, request, &pool, usable);
    if !result.is_not_found() {
        return result;
    }

    let split = split_provider_prefix(text, &pool);
    let anchor = match &constraint {
        Some(_) => pool.first().cloned(),
        None => pool
            .iter()
            .find(|model| {
                split
                    .as_ref()
                    .is_some_and(|(provider, _)| model.provider == *provider)
            })
            .cloned(),
    };
    let Some(anchor) = anchor else {
        return result;
    };
    let model_text = split
        .as_ref()
        .map(|(_, model_text)| model_text.clone())
        .unwrap_or_else(|| text.to_string());
    let suffix = if request.thinking_suffix == ThinkingSuffix::None {
        None
    } else {
        split_thinking_suffix(&model_text)
    };
    let mut provider_models = vec![anchor.clone()];
    provider_models.extend(
        pool.iter()
            .filter(|model| model.provider == anchor.provider && **model != anchor)
            .cloned(),
    );
    let model_id = suffix
        .as_ref()
        .map(|value| value.base.clone())
        .unwrap_or_else(|| model_text.clone());
    ModelReferenceResult::NotFound {
        target: Some(ModelReferenceTarget {
            provider: anchor.provider.clone(),
            provider_models,
            model_text,
            model_id,
            thinking_level: suffix.map(|value| value.thinking_level),
        }),
    }
}

/// Exact `provider/id` or bare id lookup without thinking suffixes or fuzzy
/// matching.
pub fn find_exact_model_reference_match(model_reference: &str, models: &[Model]) -> Option<Model> {
    let request = ModelReferenceRequest {
        text: model_reference,
        provider: None,
        thinking_suffix: ThinkingSuffix::None,
        fuzzy: false,
    };
    match resolve_model_reference(&request, models, &|_| true) {
        ModelReferenceResult::Resolved { model, .. } => Some(model),
        _ => None,
    }
}

fn resolve_text(
    text: &str,
    request: &ModelReferenceRequest<'_>,
    pool: &[Model],
    usable: &dyn Fn(&str) -> bool,
) -> ModelReferenceResult {
    if let Some(reference) = select_reference(text, pool, request.fuzzy, usable) {
        return reference;
    }
    if request.fuzzy
        && let Some(fuzzy) = select_fuzzy(&fuzzy_matches(text, pool), usable)
    {
        return fuzzy;
    }
    if request.thinking_suffix == ThinkingSuffix::None {
        return ModelReferenceResult::NotFound { target: None };
    }
    if let Some(thinking) = split_thinking_suffix(text) {
        let inner = resolve_text(&thinking.base, request, pool, usable);
        if inner.is_not_found() {
            return inner;
        }
        return match inner {
            ModelReferenceResult::Resolved {
                model,
                warning,
                thinking_level,
            } => ModelReferenceResult::Resolved {
                model,
                thinking_level: if warning.is_some() {
                    thinking_level
                } else {
                    Some(thinking.thinking_level)
                },
                warning,
            },
            ModelReferenceResult::Ambiguous {
                candidates,
                usable_count,
                warning,
                thinking_level,
            } => ModelReferenceResult::Ambiguous {
                candidates,
                usable_count,
                thinking_level: if warning.is_some() {
                    thinking_level
                } else {
                    Some(thinking.thinking_level)
                },
                warning,
            },
            ModelReferenceResult::NotFound { .. } => inner,
        };
    }
    // Keep an invalid suffix as part of the id and fail rather than resolve a
    // different model.
    if request.thinking_suffix == ThinkingSuffix::Strict {
        return ModelReferenceResult::NotFound { target: None };
    }
    let Some(colon) = text.rfind(':') else {
        return ModelReferenceResult::NotFound { target: None };
    };
    let inner = resolve_text(&text[..colon], request, pool, usable);
    if inner.is_not_found() {
        return inner;
    }
    let warning = format!(
        "Invalid thinking level \"{}\" in pattern \"{text}\". Using default instead.",
        &text[colon + 1..]
    );
    match inner {
        ModelReferenceResult::Resolved { model, .. } => ModelReferenceResult::Resolved {
            model,
            thinking_level: None,
            warning: Some(warning),
        },
        ModelReferenceResult::Ambiguous {
            candidates,
            usable_count,
            ..
        } => ModelReferenceResult::Ambiguous {
            candidates,
            usable_count,
            thinking_level: None,
            warning: Some(warning),
        },
        ModelReferenceResult::NotFound { .. } => inner,
    }
}

fn select_reference(
    text: &str,
    pool: &[Model],
    fuzzy: bool,
    usable: &dyn Fn(&str) -> bool,
) -> Option<ResolvedOrAmbiguous> {
    let lower_text = text.to_lowercase();
    let raw: Vec<Model> = pool
        .iter()
        .filter(|model| model.id.to_lowercase() == lower_text)
        .cloned()
        .collect();
    let split = split_provider_prefix(text, pool);
    let mut named: Option<Model> = None;
    if let Some((provider, model_text)) = &split {
        let provider_models: Vec<Model> = pool
            .iter()
            .filter(|model| model.provider == *provider)
            .cloned()
            .collect();
        let lower_model_text = model_text.to_lowercase();
        named = provider_models
            .iter()
            .find(|model| model.id.to_lowercase() == lower_model_text)
            .cloned()
            .or_else(|| {
                raw.iter()
                    .find(|model| model.provider == *provider)
                    .cloned()
            })
            .or_else(|| {
                if fuzzy {
                    best_fuzzy_match(&fuzzy_matches(model_text, &provider_models))
                } else {
                    None
                }
            });
    }

    let others: Vec<Model> = raw
        .into_iter()
        .filter(|model| named.as_ref().is_none_or(|named| model != named))
        .collect();
    let mut candidates = Vec::new();
    if let Some(named) = &named {
        candidates.push(named.clone());
    }
    candidates.extend(others.iter().cloned());
    if candidates.is_empty() {
        return None;
    }
    let providers: std::collections::BTreeSet<&str> = candidates
        .iter()
        .map(|model| model.provider.as_str())
        .collect();
    if providers.len() == 1 {
        return Some(ModelReferenceResult::Resolved {
            model: candidates[0].clone(),
            thinking_level: None,
            warning: None,
        });
    }
    if let Some(named) = &named
        && usable(&named.provider)
    {
        return Some(ModelReferenceResult::Resolved {
            model: named.clone(),
            thinking_level: None,
            warning: None,
        });
    }
    let usable_others: Vec<&Model> = others
        .iter()
        .filter(|model| usable(&model.provider))
        .collect();
    if usable_others.len() == 1 {
        return Some(ModelReferenceResult::Resolved {
            model: usable_others[0].clone(),
            thinking_level: None,
            warning: None,
        });
    }
    if usable_others.is_empty()
        && let Some(named) = &named
    {
        return Some(ModelReferenceResult::Resolved {
            model: named.clone(),
            thinking_level: None,
            warning: None,
        });
    }
    Some(ModelReferenceResult::Ambiguous {
        candidates,
        usable_count: usable_others.len(),
        thinking_level: None,
        warning: None,
    })
}

fn fuzzy_matches(text: &str, models: &[Model]) -> Vec<Model> {
    let lower = text.to_lowercase();
    models
        .iter()
        .filter(|model| {
            model.id.to_lowercase().contains(&lower) || model.name.to_lowercase().contains(&lower)
        })
        .cloned()
        .collect()
}

fn select_fuzzy(matches: &[Model], usable: &dyn Fn(&str) -> bool) -> Option<ResolvedOrAmbiguous> {
    if matches.is_empty() {
        return None;
    }
    let providers: std::collections::BTreeSet<&str> = matches
        .iter()
        .map(|model| model.provider.as_str())
        .collect();
    let multiple = providers.len() > 1;
    let usable_matches: Vec<Model> = if multiple {
        matches
            .iter()
            .filter(|model| usable(&model.provider))
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    let pool = if usable_matches.is_empty() {
        matches.to_vec()
    } else {
        usable_matches.clone()
    };
    let best = best_fuzzy_match(&pool)?;
    let tied: Vec<Model> = pool
        .iter()
        .filter(|model| model.id == best.id)
        .cloned()
        .collect();
    let tied_providers: std::collections::BTreeSet<&str> =
        tied.iter().map(|model| model.provider.as_str()).collect();
    if tied_providers.len() == 1 {
        return Some(ModelReferenceResult::Resolved {
            model: best,
            thinking_level: None,
            warning: None,
        });
    }
    Some(ModelReferenceResult::Ambiguous {
        candidates: tied,
        usable_count: if usable_matches.is_empty() {
            0
        } else {
            usable_matches.len()
        },
        thinking_level: None,
        warning: None,
    })
}

fn is_alias(id: &str) -> bool {
    if id.ends_with("-latest") {
        return true;
    }
    let digits = id.rsplit('-').next().unwrap_or(id);
    !(digits.len() == 8 && digits.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Prefer aliases over dated versions, then the lexicographically latest id.
pub fn best_fuzzy_match(matches: &[Model]) -> Option<Model> {
    let aliases: Vec<Model> = matches
        .iter()
        .filter(|model| is_alias(&model.id))
        .cloned()
        .collect();
    let pool = if aliases.is_empty() {
        matches.to_vec()
    } else {
        aliases
    };
    pool.into_iter().max_by(|a, b| a.id.cmp(&b.id))
}

fn build_fallback_model(provider_models: &[Model], model_id: &str) -> Option<Model> {
    let first = provider_models.first()?;
    let default_id = default_model_per_provider(&first.provider);
    let base = default_id
        .and_then(|id| provider_models.iter().find(|model| model.id == id))
        .unwrap_or(first);
    let mut model = base.clone();
    model.id = model_id.to_string();
    model.name = model_id.to_string();
    Some(model)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticCode {
    NoMatch,
    InvalidThinkingLevel,
    Ambiguous,
}

impl DiagnosticCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoMatch => "no-match",
            Self::InvalidThinkingLevel => "invalid-thinking-level",
            Self::Ambiguous => "ambiguous",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelScopeDiagnostic {
    pub diagnostic_type: &'static str,
    pub code: DiagnosticCode,
    pub message: String,
    pub pattern: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResolveModelScopeResult {
    pub scoped_models: Vec<ScopedModel>,
    pub diagnostics: Vec<ModelScopeDiagnostic>,
}

fn glob_matches(pattern: &str, models: &[Model]) -> Result<Vec<Model>, String> {
    let matcher = GlobBuilder::new(pattern)
        .case_insensitive(true)
        .literal_separator(true)
        .build()
        .map_err(|error| error.to_string())?
        .compile_matcher();
    Ok(models
        .iter()
        .filter(|model| {
            matcher.is_match(format!("{}/{}", model.provider, model.id))
                || matcher.is_match(&model.id)
        })
        .cloned()
        .collect())
}

/// Match a glob pattern against both `provider/model` and bare model ids.
pub fn find_matching_models(pattern: &str, models: &[Model]) -> Vec<Model> {
    glob_matches(pattern, models).unwrap_or_default()
}

fn scope_add(
    result: &mut ResolveModelScopeResult,
    model: Model,
    thinking_level: Option<ThinkingLevel>,
) {
    if !result
        .scoped_models
        .iter()
        .any(|scoped| scoped.model == model)
    {
        result.scoped_models.push(ScopedModel {
            model,
            thinking_level,
        });
    }
}

fn scope_no_match(result: &mut ResolveModelScopeResult, pattern: &str) {
    result.diagnostics.push(ModelScopeDiagnostic {
        diagnostic_type: "warning",
        code: DiagnosticCode::NoMatch,
        message: format!("No models match pattern \"{pattern}\""),
        pattern: pattern.to_string(),
    });
}

fn scope_ambiguous(result: &mut ResolveModelScopeResult, pattern: &str, candidates: &[Model]) {
    let mut providers: Vec<&str> = candidates
        .iter()
        .map(|model| model.provider.as_str())
        .collect();
    providers.sort_unstable();
    providers.dedup();
    result.diagnostics.push(ModelScopeDiagnostic {
        diagnostic_type: "warning",
        code: DiagnosticCode::Ambiguous,
        message: format!(
            "Pattern \"{pattern}\" matches several providers: {}. Use provider/id to choose one.",
            providers.join(", ")
        ),
        pattern: pattern.to_string(),
    });
}

fn scope_invalid_warning(result: &mut ResolveModelScopeResult, pattern: &str, warning: String) {
    result.diagnostics.push(ModelScopeDiagnostic {
        diagnostic_type: "warning",
        code: DiagnosticCode::InvalidThinkingLevel,
        message: warning,
        pattern: pattern.to_string(),
    });
}

/// Resolve `--models`/`enabledModels` patterns to models with optional thinking
/// levels. Every model is treated as usable, so credentials cannot choose
/// between providers offering the same id; all are added with a warning.
pub fn resolve_model_scope_from_models(
    patterns: &[String],
    models: &[Model],
) -> ResolveModelScopeResult {
    let mut result = ResolveModelScopeResult::default();

    for pattern in patterns {
        if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
            let thinking = split_thinking_suffix(pattern);
            let thinking_level = thinking.as_ref().map(|value| value.thinking_level);
            let glob_pattern = thinking
                .as_ref()
                .map(|value| value.base.as_str())
                .unwrap_or(pattern.as_str());
            let exact = resolve_model_reference(
                &ModelReferenceRequest {
                    text: glob_pattern,
                    provider: None,
                    thinking_suffix: ThinkingSuffix::None,
                    fuzzy: false,
                },
                models,
                &|_| true,
            );
            let matches: Vec<Model> = match exact {
                ModelReferenceResult::Resolved { model, .. } => vec![model],
                ModelReferenceResult::Ambiguous { candidates, .. } => {
                    scope_ambiguous(&mut result, pattern, &candidates);
                    candidates
                }
                ModelReferenceResult::NotFound { .. } => find_matching_models(glob_pattern, models),
            };
            if matches.is_empty() {
                scope_no_match(&mut result, pattern);
            }
            for model in matches {
                scope_add(&mut result, model, thinking_level);
            }
            continue;
        }

        let resolved = resolve_model_reference(
            &ModelReferenceRequest {
                text: pattern,
                provider: None,
                thinking_suffix: ThinkingSuffix::Lenient,
                fuzzy: true,
            },
            models,
            &|_| true,
        );
        match resolved {
            ModelReferenceResult::NotFound { .. } => {
                scope_no_match(&mut result, pattern);
            }
            ModelReferenceResult::Resolved {
                model,
                thinking_level,
                warning,
            } => {
                if let Some(warning) = warning {
                    scope_invalid_warning(&mut result, pattern, warning);
                }
                scope_add(&mut result, model, thinking_level);
            }
            ModelReferenceResult::Ambiguous {
                candidates,
                thinking_level,
                warning,
                ..
            } => {
                if let Some(warning) = warning {
                    scope_invalid_warning(&mut result, pattern, warning);
                }
                scope_ambiguous(&mut result, pattern, &candidates);
                for model in candidates {
                    scope_add(&mut result, model, thinking_level);
                }
            }
        }
    }
    result
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResolveCliModelResult {
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
    pub warning: Option<String>,
    pub error: Option<String>,
}

/// Options for [`resolve_cli_model`]. The predicate decides which providers have
/// stored credentials; an inline `--api-key` passes `false` so it cannot select
/// a provider.
pub struct CliModelOptions<'a> {
    pub cli_provider: Option<&'a str>,
    pub cli_model: Option<&'a str>,
    pub cli_thinking: Option<ThinkingLevel>,
    pub inline_api_key: bool,
    pub models: &'a [Model],
    pub has_configured_auth: &'a dyn Fn(&str) -> bool,
}

/// Resolve a single model from `--provider`/`--model` flags. It only parses a
/// thinking level from `<pattern>:<thinking>`; applying it is the caller's job.
pub fn resolve_cli_model(options: &CliModelOptions<'_>) -> ResolveCliModelResult {
    let Some(cli_model) = options.cli_model else {
        return ResolveCliModelResult::default();
    };
    if options.models.is_empty() {
        return ResolveCliModelResult {
            error: Some(
                "No models available. Check your installation or add models to models.json.".into(),
            ),
            ..ResolveCliModelResult::default()
        };
    }
    let provider = options.cli_provider.and_then(|requested| {
        options
            .models
            .iter()
            .find(|model| model.provider.to_lowercase() == requested.to_lowercase())
            .map(|model| model.provider.clone())
    });
    if options.cli_provider.is_some() && provider.is_none() {
        return ResolveCliModelResult {
            error: Some(format!(
                "Unknown provider \"{}\". Use --list-models to see available providers/models.",
                options.cli_provider.unwrap_or_default()
            )),
            ..ResolveCliModelResult::default()
        };
    }
    let usable: &dyn Fn(&str) -> bool = if options.inline_api_key {
        &|_| false
    } else {
        options.has_configured_auth
    };
    let request = ModelReferenceRequest {
        text: cli_model,
        provider: provider.as_deref(),
        thinking_suffix: ThinkingSuffix::Strict,
        fuzzy: true,
    };
    let resolved = resolve_model_reference(&request, options.models, usable);
    match resolved {
        ModelReferenceResult::Resolved {
            model,
            thinking_level,
            warning,
        } => ResolveCliModelResult {
            model: Some(model),
            thinking_level,
            warning,
            error: None,
        },
        ModelReferenceResult::Ambiguous {
            candidates,
            usable_count,
            ..
        } => {
            let mut matches: Vec<String> = candidates
                .iter()
                .map(|model| format!("{}/{}", model.provider, model.id))
                .collect();
            matches.sort_unstable();
            let auth_hint = if options.inline_api_key {
                "--api-key does not choose a provider."
            } else if usable_count == 0 {
                "No matching provider is authenticated."
            } else {
                "More than one matching provider is authenticated."
            };
            ResolveCliModelResult {
                error: Some(format!(
                    "Model \"{cli_model}\" is ambiguous across providers: {}. {auth_hint} Use --provider or provider/model.",
                    matches.join(", ")
                )),
                ..ResolveCliModelResult::default()
            }
        }
        ModelReferenceResult::NotFound { target: None } => ResolveCliModelResult {
            error: Some(format!(
                "Model \"{cli_model}\" not found. Use --list-models to see available models."
            )),
            ..ResolveCliModelResult::default()
        },
        ModelReferenceResult::NotFound {
            target: Some(target),
        } => {
            // When --thinking is explicit the suffix stays in the custom id.
            let model_id = if options.cli_thinking.is_some() {
                target.model_text.clone()
            } else {
                target.model_id.clone()
            };
            let fallback_thinking = if options.cli_thinking.is_some() {
                None
            } else {
                target.thinking_level
            };
            let Some(mut model) = build_fallback_model(&target.provider_models, &model_id) else {
                return ResolveCliModelResult {
                    error: Some(format!("Model \"{cli_model}\" not found.")),
                    ..ResolveCliModelResult::default()
                };
            };
            let requested_thinking = options.cli_thinking.or(fallback_thinking);
            if requested_thinking.is_some_and(|level| level != ThinkingLevel::Off) {
                model.reasoning = true;
            }
            ResolveCliModelResult {
                model: Some(model),
                thinking_level: fallback_thinking,
                warning: Some(format!(
                    "Model \"{model_id}\" not found for provider \"{}\". Using custom model id.",
                    target.provider
                )),
                error: None,
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct InitialModelResult {
    pub model: Option<Model>,
    pub thinking_level: ThinkingLevel,
    pub fallback_message: Option<String>,
}

/// Options for [`find_initial_model`].
pub struct InitialModelOptions<'a> {
    pub scoped_models: &'a [ScopedModel],
    pub is_continuing: bool,
    pub default_provider: Option<&'a str>,
    pub default_model_id: Option<&'a str>,
    pub default_thinking_level: Option<ThinkingLevel>,
    pub model_thinking_levels: Option<&'a std::collections::HashMap<String, ThinkingLevel>>,
    pub models: &'a [Model],
    pub available: &'a [Model],
    pub has_configured_auth: &'a dyn Fn(&str) -> bool,
}

/// Pick the initial model: first scoped model, then saved default with working
/// credentials, then the first available provider default, then any available
/// model.
pub fn find_initial_model(options: &InitialModelOptions<'_>) -> InitialModelResult {
    let mut thinking_level = DEFAULT_THINKING_LEVEL;
    if !options.scoped_models.is_empty()
        && !options.is_continuing
        && let Some(scoped) = options.scoped_models.first()
    {
        let key = format!("{}/{}", scoped.model.provider, scoped.model.id);
        let level = scoped
            .thinking_level
            .or_else(|| {
                options
                    .model_thinking_levels
                    .and_then(|map| map.get(&key).copied())
            })
            .or(options.default_thinking_level)
            .unwrap_or(DEFAULT_THINKING_LEVEL);
        return InitialModelResult {
            model: Some(scoped.model.clone()),
            thinking_level: level,
            fallback_message: None,
        };
    }
    if let (Some(provider), Some(id)) = (options.default_provider, options.default_model_id)
        && let Some(found) = options
            .models
            .iter()
            .find(|model| model.provider == provider && model.id == id)
        && (options.has_configured_auth)(&found.provider)
    {
        let key = format!("{provider}/{id}");
        if let Some(level) = options.model_thinking_levels.and_then(|map| map.get(&key)) {
            thinking_level = *level;
        } else if let Some(level) = options.default_thinking_level {
            thinking_level = level;
        }
        return InitialModelResult {
            model: Some(found.clone()),
            thinking_level,
            fallback_message: None,
        };
    }
    if !options.available.is_empty() {
        for (provider, default_id) in DEFAULT_MODEL_PER_PROVIDER {
            if let Some(match_) = options
                .available
                .iter()
                .find(|model| model.provider == *provider && model.id == *default_id)
            {
                return InitialModelResult {
                    model: Some(match_.clone()),
                    thinking_level: DEFAULT_THINKING_LEVEL,
                    fallback_message: None,
                };
            }
        }
        return InitialModelResult {
            model: options.available.first().cloned(),
            thinking_level: DEFAULT_THINKING_LEVEL,
            fallback_message: None,
        };
    }
    InitialModelResult {
        model: None,
        thinking_level: DEFAULT_THINKING_LEVEL,
        fallback_message: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_rs_ai::{Api, BaseModel, ModelCost, ModelCostRates, ModelInputModality};

    fn model(provider: &str, id: &str) -> Model {
        model_with_name(provider, id, id)
    }

    fn model_with_name(provider: &str, id: &str, name: &str) -> Model {
        Model {
            base: BaseModel {
                id: id.into(),
                name: name.into(),
                api: Api::OpenAiCompletions,
                provider: provider.into(),
                base_url: "https://example.invalid".into(),
                input: vec![ModelInputModality::Text],
                input_limits: None,
                cost: ModelCost {
                    rates: ModelCostRates::default(),
                    tiers: Vec::new(),
                },
                headers: None,
            },
            kind: None,
            reasoning: true,
            thinking_level_map: None,
            prompt_cache: None,
            context_window: 128_000,
            max_tokens: 8_192,
            sampling_params: None,
            sampling_params_by_thinking_level: None,
            compat: None,
        }
    }

    fn catalog() -> Vec<Model> {
        vec![
            model_with_name("anthropic", "claude-sonnet-4-5", "Claude Sonnet 4.5"),
            model("anthropic", "claude-sonnet-4-5-20250929"),
            model("anthropic", "claude-opus-4-8"),
            model("openai", "gpt-4o"),
            model("openrouter", "qwen/qwen3-coder:exacto"),
            model("gw-a", "shared"),
            model("gw-b", "shared"),
            model("llama=http://host:10800", "unsloth/qwen-gguf:BF16"),
        ]
    }

    fn reference(result: &ModelReferenceResult) -> String {
        match result {
            ModelReferenceResult::Resolved { model, .. } => {
                format!("{}/{}", model.provider, model.id)
            }
            ModelReferenceResult::Ambiguous { candidates, .. } => candidates
                .iter()
                .map(|model| format!("{}/{}", model.provider, model.id))
                .collect::<Vec<_>>()
                .join(","),
            ModelReferenceResult::NotFound { .. } => "not-found".into(),
        }
    }

    fn resolve(text: &str, usable: &[&str]) -> ModelReferenceResult {
        resolve_model_reference(&ModelReferenceRequest::new(text), &catalog(), &|provider| {
            usable.contains(&provider)
        })
    }

    #[test]
    fn parses_all_thinking_levels() {
        for level in ThinkingLevel::ALL {
            assert_eq!(parse_thinking_level(level.as_str()), Some(level));
        }
        assert_eq!(parse_thinking_level("random"), None);
    }

    #[test]
    fn exact_id_is_case_insensitive() {
        assert_eq!(reference(&resolve("GPT-4O", &[])), "openai/gpt-4o");
    }

    #[test]
    fn fuzzy_prefers_alias_over_dated_id() {
        assert_eq!(
            reference(&resolve("sonnet", &[])),
            "anthropic/claude-sonnet-4-5"
        );
    }

    #[test]
    fn thinking_suffix_is_parsed_after_exact_match() {
        let result = resolve("sonnet:high", &[]);
        assert_eq!(reference(&result), "anthropic/claude-sonnet-4-5");
        assert!(matches!(
            result,
            ModelReferenceResult::Resolved {
                thinking_level: Some(ThinkingLevel::High),
                ..
            }
        ));
    }

    #[test]
    fn colon_inside_literal_id_is_not_a_thinking_suffix() {
        assert_eq!(
            reference(&resolve("qwen/qwen3-coder:exacto", &[])),
            "openrouter/qwen/qwen3-coder:exacto"
        );
    }

    #[test]
    fn provider_prefix_colon_id_and_thinking_suffix() {
        let result = resolve("openrouter/qwen/qwen3-coder:exacto:high", &[]);
        assert_eq!(reference(&result), "openrouter/qwen/qwen3-coder:exacto");
        assert!(matches!(
            result,
            ModelReferenceResult::Resolved {
                thinking_level: Some(ThinkingLevel::High),
                ..
            }
        ));
    }

    #[test]
    fn provider_id_with_slashes_is_matched_longest_first() {
        let id = "llama=http://host:10800/unsloth/qwen-gguf:BF16";
        assert_eq!(reference(&resolve(id, &[])), id);
    }

    #[test]
    fn strict_keeps_invalid_suffix_in_id() {
        let result = resolve("gpt-4o:random", &[]);
        assert!(result.is_not_found());
    }

    #[test]
    fn lenient_drops_invalid_suffix_with_warning() -> Result<(), String> {
        let request = ModelReferenceRequest {
            text: "gpt-4o:random",
            provider: None,
            thinking_suffix: ThinkingSuffix::Lenient,
            fuzzy: true,
        };
        let result = resolve_model_reference(&request, &catalog(), &|_| true);
        match &result {
            ModelReferenceResult::Resolved { model, warning, .. } => {
                assert_eq!(model.id, "gpt-4o");
                assert!(
                    warning
                        .as_deref()
                        .is_some_and(|w| w.contains("Invalid thinking level \"random\""))
                );
            }
            _ => return Err("expected resolved model".to_string()),
        }
        Ok(())
    }

    #[test]
    fn suffix_mode_none_keeps_suffix_in_id() {
        let request = ModelReferenceRequest {
            text: "gpt-4o:high",
            provider: None,
            thinking_suffix: ThinkingSuffix::None,
            fuzzy: true,
        };
        assert!(resolve_model_reference(&request, &catalog(), &|_| true).is_not_found());
    }

    #[test]
    fn sole_usable_provider_wins_over_ambiguity() {
        assert_eq!(reference(&resolve("shared", &["gw-b"])), "gw-b/shared");
    }

    #[test]
    fn no_usable_provider_is_ambiguous() -> Result<(), String> {
        let result = resolve("shared", &[]);
        match &result {
            ModelReferenceResult::Ambiguous {
                candidates,
                usable_count,
                ..
            } => {
                assert_eq!(candidates.len(), 2);
                assert_eq!(*usable_count, 0);
            }
            _ => return Err("expected ambiguity".to_string()),
        }
        Ok(())
    }

    #[test]
    fn thinking_suffix_does_not_bypass_ambiguity() {
        let result = resolve("shared:high", &["gw-a", "gw-b"]);
        assert!(matches!(result, ModelReferenceResult::Ambiguous { .. }));
    }

    #[test]
    fn raw_id_beats_unusable_qualified_match() {
        let models = vec![
            model("anthropic", "exact"),
            model("gw-a", "anthropic/exact"),
        ];
        let result = resolve_model_reference(
            &ModelReferenceRequest::new("anthropic/exact"),
            &models,
            &|provider| provider == "gw-a",
        );
        assert_eq!(reference(&result), "gw-a/anthropic/exact");
    }

    #[test]
    fn find_exact_match_ignores_fuzzy_and_suffixes() {
        let models = catalog();
        let found = find_exact_model_reference_match("gpt-4o", &models);
        assert!(found.is_some_and(|model| model.provider == "openai"));
        assert!(find_exact_model_reference_match("sonnet", &models).is_none());
    }

    #[test]
    fn glob_matches_provider_and_bare_id_case_insensitively() {
        let models = catalog();
        let matches = find_matching_models("ANTHROPIC/*SONNET*", &models);
        assert_eq!(matches.len(), 2);
        let matches = find_matching_models("*sonnet*", &models);
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn scoped_patterns_report_diagnostics() {
        let patterns = vec![
            "sonnet:high".to_string(),
            "gpt-4o:invalid".to_string(),
            "missing".to_string(),
        ];
        let result = resolve_model_scope_from_models(&patterns, &catalog());
        assert_eq!(result.scoped_models.len(), 2);
        assert_eq!(
            result.scoped_models[0].thinking_level,
            Some(ThinkingLevel::High)
        );
        assert_eq!(
            result
                .diagnostics
                .iter()
                .map(|d| d.code)
                .collect::<Vec<_>>(),
            vec![
                DiagnosticCode::InvalidThinkingLevel,
                DiagnosticCode::NoMatch
            ]
        );
    }

    #[test]
    fn scoped_ambiguous_pattern_keeps_every_provider() {
        let models = vec![model("gw-a", "shared"), model("gw-b", "shared")];
        let result = resolve_model_scope_from_models(&["shared".to_string()], &models);
        assert_eq!(result.scoped_models.len(), 2);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == DiagnosticCode::Ambiguous)
        );
    }

    #[test]
    fn bracketed_ids_are_resolved_before_glob_matching() {
        let models = vec![model("custom", "bracketed-model[1m]")];
        let result = resolve_model_scope_from_models(
            &["custom/bracketed-model[1m]:high".to_string()],
            &models,
        );
        assert_eq!(result.scoped_models.len(), 1);
        assert_eq!(result.scoped_models[0].model.id, "bracketed-model[1m]");
        assert_eq!(
            result.scoped_models[0].thinking_level,
            Some(ThinkingLevel::High)
        );
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn cli_model_falls_back_to_a_custom_id_without_leaking_thinking_suffix() -> Result<(), String> {
        let mut models = catalog();
        models.push(model("neuralwatt", "some-base-model"));
        let auth = |_provider: &str| false;
        let result = resolve_cli_model(&CliModelOptions {
            cli_provider: None,
            cli_model: Some("neuralwatt/zai-org/GLM-5.1-FP8:high"),
            cli_thinking: None,
            inline_api_key: false,
            models: &models,
            has_configured_auth: &auth,
        });
        assert_eq!(result.error, None);
        let resolved = result.model.ok_or("missing fallback model")?;
        assert_eq!(resolved.provider, "neuralwatt");
        assert_eq!(resolved.id, "zai-org/GLM-5.1-FP8");
        assert!(resolved.reasoning);
        assert_eq!(result.thinking_level, Some(ThinkingLevel::High));
        Ok(())
    }

    #[test]
    fn explicit_thinking_keeps_suffix_in_custom_id() {
        let mut models = catalog();
        models.push(model("neuralwatt", "some-base-model"));
        let auth = |_provider: &str| false;
        let result = resolve_cli_model(&CliModelOptions {
            cli_provider: None,
            cli_model: Some("neuralwatt/custom:high"),
            cli_thinking: Some(ThinkingLevel::Medium),
            inline_api_key: false,
            models: &models,
            has_configured_auth: &auth,
        });
        assert_eq!(
            result.model.as_ref().map(|model| model.id.as_str()),
            Some("custom:high")
        );
        assert_eq!(result.thinking_level, None);
    }

    #[test]
    fn inline_api_key_does_not_let_stored_credentials_choose_provider() {
        let models = vec![model("anthropic", "dup"), model("gateway", "dup")];
        let auth = |provider: &str| provider == "gateway";
        let result = resolve_cli_model(&CliModelOptions {
            cli_provider: None,
            cli_model: Some("dup"),
            cli_thinking: None,
            inline_api_key: true,
            models: &models,
            has_configured_auth: &auth,
        });
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|e| e.contains("--api-key does not choose a provider"))
        );
    }

    #[test]
    fn initial_model_prefers_first_scoped_model() {
        let scoped = vec![ScopedModel {
            model: model("custom", "chosen"),
            thinking_level: Some(ThinkingLevel::High),
        }];
        let auth = |_provider: &str| false;
        let result = find_initial_model(&InitialModelOptions {
            scoped_models: &scoped,
            is_continuing: false,
            default_provider: None,
            default_model_id: None,
            default_thinking_level: None,
            model_thinking_levels: None,
            models: &[],
            available: &[],
            has_configured_auth: &auth,
        });
        assert_eq!(
            result.model.as_ref().map(|model| model.id.as_str()),
            Some("chosen")
        );
        assert_eq!(result.thinking_level, ThinkingLevel::High);
    }

    #[test]
    fn default_model_table_tracks_expected_ids() {
        assert_eq!(default_model_per_provider("openai"), Some("gpt-6.2-sol"));
        assert_eq!(
            default_model_per_provider("vercel-ai-gateway"),
            Some("zai/glm-5.3")
        );
        assert_eq!(default_model_per_provider("unknown"), None);
    }
}
