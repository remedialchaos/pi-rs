use std::sync::Arc;

use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::error::Error;
use crate::model::{CacheRetention, Model, SamplingParams, ThinkingLevel};

pub type ProviderEnv = IndexMap<String, String>;
pub type ProviderHeaders = IndexMap<String, Option<String>>;
pub type Observer<T, M> =
    Arc<dyn for<'a> Fn(&'a T, &'a M) -> BoxFuture<'a, Result<(), Error>> + Send + Sync>;
pub type PayloadTransform<M> =
    Arc<dyn for<'a> Fn(Value, &'a M) -> BoxFuture<'a, Result<Value, Error>> + Send + Sync>;
pub type HeadersTransform = Arc<
    dyn Fn(ProviderHeaders) -> BoxFuture<'static, Result<ProviderHeaders, Error>> + Send + Sync,
>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attribution {
    pub name: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub categories: Option<Vec<String>>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserAgent {
    Detailed,
    Short,
    Custom(String),
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientIdentity {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<UserAgent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// None inherits defaults; Some(None) suppresses inherited attribution.
    #[serde(
        default,
        deserialize_with = "deserialize_attribution",
        skip_serializing_if = "Option::is_none"
    )]
    pub attribution: Option<Option<Attribution>>,
}
fn deserialize_attribution<'de, D: serde::Deserializer<'de>>(
    de: D,
) -> Result<Option<Option<Attribution>>, D::Error> {
    Option::<Attribution>::deserialize(de).map(Some)
}
#[derive(Clone, Debug)]
pub struct ProviderResponse {
    pub status: u16,
    pub headers: IndexMap<String, String>,
}

pub struct ProviderRequestOptions<M = Model> {
    pub signal: CancellationToken,
    pub api_key: Option<String>,
    pub env: ProviderEnv,
    pub headers: ProviderHeaders,
    pub identity: ClientIdentity,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<u64>,
    pub client: Option<reqwest::Client>,
    pub on_payload: Option<PayloadTransform<M>>,
    pub on_response: Option<Observer<ProviderResponse, M>>,
    pub transform_headers: Option<HeadersTransform>,
}

impl<M> Default for ProviderRequestOptions<M> {
    fn default() -> Self {
        Self {
            signal: CancellationToken::new(),
            api_key: None,
            env: ProviderEnv::new(),
            headers: ProviderHeaders::new(),
            identity: ClientIdentity::default(),
            timeout_ms: None,
            max_retries: None,
            max_retry_delay_ms: None,
            client: None,
            on_payload: None,
            on_response: None,
            transform_headers: None,
        }
    }
}
impl<M> Clone for ProviderRequestOptions<M> {
    fn clone(&self) -> Self {
        Self {
            signal: self.signal.clone(),
            api_key: self.api_key.clone(),
            env: self.env.clone(),
            headers: self.headers.clone(),
            identity: self.identity.clone(),
            timeout_ms: self.timeout_ms,
            max_retries: self.max_retries,
            max_retry_delay_ms: self.max_retry_delay_ms,
            client: self.client.clone(),
            on_payload: self.on_payload.clone(),
            on_response: self.on_response.clone(),
            transform_headers: self.transform_headers.clone(),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    Sse,
    Websocket,
    WebsocketCached,
    Auto,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolChoice {
    Auto,
    None,
}
#[derive(Clone, Default)]
pub struct StreamOptions {
    pub request: ProviderRequestOptions,
    pub telemetry: Option<pi_rs_telemetry::Context>,
    pub on_provider_stream_event: Option<Observer<Value, Model>>,
    pub temperature: Option<f64>,
    pub sampling_params: SamplingParams,
    pub max_tokens: Option<u64>,
    pub transport: Option<Transport>,
    pub cache_retention: Option<CacheRetention>,
    pub session_id: Option<String>,
    pub disable_continuation: bool,
    pub websocket_connect_timeout_ms: Option<u64>,
    pub metadata: SamplingParams,
    pub tool_choice: Option<ToolChoice>,
    pub reasoning: Option<ThinkingLevel>,
    pub thinking_budgets: IndexMap<ThinkingLevel, u64>,
    pub deferred: Option<Value>,
    /// Wire-API-specific options, independent of arbitrary sampling overrides.
    pub specific: SamplingParams,
}

pub fn provider_env_value<'e>(
    name: &str,
    env: &'e ProviderEnv,
) -> Option<std::borrow::Cow<'e, str>> {
    env.get(name)
        .filter(|s| !s.is_empty())
        .map(|s| std::borrow::Cow::Borrowed(s.as_str()))
        .or_else(|| {
            std::env::var(name)
                .ok()
                .filter(|s| !s.is_empty())
                .map(std::borrow::Cow::Owned)
        })
}

pub fn merge_headers(mut lower: ProviderHeaders, upper: ProviderHeaders) -> ProviderHeaders {
    for (name, value) in upper {
        lower.retain(|existing, _| !existing.eq_ignore_ascii_case(&name));
        lower.insert(name, value);
    }
    lower
}
