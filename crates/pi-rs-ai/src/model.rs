use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

macro_rules! wire_id {
    ($name:ident { $($variant:ident => $id:literal),+ $(,)? }) => {
        #[derive(Clone, Debug, Eq, PartialEq, Hash)]
        #[non_exhaustive]
        pub enum $name {
            $($variant,)+
            Custom(String),
        }
        impl $name {
            pub fn as_str(&self) -> &str {
                match self { $(Self::$variant => $id,)+ Self::Custom(name) => name }
            }
        }
        impl From<String> for $name {
            fn from(id: String) -> Self {
                match id.as_str() { $($id => Self::$variant,)+ _ => Self::Custom(id) }
            }
        }
        impl From<&str> for $name {
            fn from(id: &str) -> Self {
                match id { $($id => Self::$variant,)+ _ => Self::Custom(id.to_owned()) }
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                String::deserialize(deserializer).map(Self::from)
            }
        }
    };
}

wire_id!(Api {
    AnthropicMessages => "anthropic-messages",
    OpenAiCompletions => "openai-completions",
    OpenAiResponses => "openai-responses",
    AzureOpenAiResponses => "azure-openai-responses",
    OpenAiCodexResponses => "openai-codex-responses",
    BedrockConverseStream => "bedrock-converse-stream",
    GoogleGenerativeAi => "google-generative-ai",
    GoogleVertex => "google-vertex",
    MistralConversations => "mistral-conversations",
    PiMessages => "pi-messages",
});
wire_id!(ImageApi { OpenRouterImages => "openrouter-images" });
wire_id!(ClassifierApi {
    TypeSafeSystemOne => "typesafe-system-one",
    CloudflareWorkersAiSystemOne => "cloudflare-workers-ai-system-one",
    LlamaCppClassify => "llama-cpp-classify",
    OpenAiDecisions => "openai-decisions",
});

pub type ProviderId = String;
pub type SamplingParams = Map<String, Value>;
pub type SamplingParamsByThinkingLevel = IndexMap<ThinkingLevel, SamplingParams>;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingLevel {
    pub const ALL: [Self; 7] = [
        Self::Off,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Xhigh,
        Self::Max,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheRetention {
    None,
    #[default]
    Short,
    Long,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelInputModality {
    Text,
    Image,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelType {
    Chat,
    Image,
    Classifier,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelPromptCache {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub long: Option<f64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostRates {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostTier {
    pub input_tokens_above: f64,
    #[serde(flatten)]
    pub rates: ModelCostRates,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelCost {
    #[serde(flatten)]
    pub rates: ModelCostRates,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tiers: Vec<ModelCostTier>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelImageResizeOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_width: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_height: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jpeg_quality: Option<u8>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelImageInputLimits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resize: Option<ModelImageResizeOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_per_message: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_per_request: Option<u64>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInputLimits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_request_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<ModelImageInputLimits>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BaseModel<A> {
    pub id: String,
    pub name: String,
    pub api: A,
    pub provider: ProviderId,
    pub base_url: String,
    pub input: Vec<ModelInputModality>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_limits: Option<ModelInputLimits>,
    pub cost: ModelCost,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<IndexMap<String, String>>,
}

macro_rules! model_tag {
    ($name:ident, $variant:ident, $tag:literal) => {
        #[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
        pub enum $name {
            #[default]
            #[serde(rename = $tag)]
            $variant,
        }
    };
}
model_tag!(ChatKind, Chat, "chat");
model_tag!(ImageKind, Image, "image");
model_tag!(ClassifierKind, Classifier, "classifier");

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    #[serde(flatten)]
    pub base: BaseModel<Api>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<ChatKind>,
    pub reasoning: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<IndexMap<ThinkingLevel, Option<String>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache: Option<ModelPromptCache>,
    pub context_window: u64,
    pub max_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampling_params: Option<SamplingParams>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampling_params_by_thinking_level: Option<SamplingParamsByThinkingLevel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compat: Option<Map<String, Value>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageModel {
    #[serde(flatten)]
    pub base: BaseModel<ImageApi>,
    #[serde(rename = "type")]
    pub kind: ImageKind,
    pub output: Vec<ModelInputModality>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClassifierModel {
    #[serde(flatten)]
    pub base: BaseModel<ClassifierApi>,
    #[serde(rename = "type")]
    pub kind: ClassifierKind,
    pub context_window: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compat: Option<Map<String, Value>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
#[expect(
    clippy::large_enum_variant,
    reason = "Catalog variants stay inline; no per-model allocation or pointer chase."
)]
pub enum AnyModel {
    Image(ImageModel),
    Classifier(ClassifierModel),
    Chat(Model),
}

macro_rules! base_access {
    ($name:ident, $api:ident) => {
        impl std::ops::Deref for $name {
            type Target = BaseModel<$api>;
            fn deref(&self) -> &Self::Target {
                &self.base
            }
        }
        impl std::ops::DerefMut for $name {
            fn deref_mut(&mut self) -> &mut Self::Target {
                &mut self.base
            }
        }
    };
}
base_access!(Model, Api);
base_access!(ImageModel, ImageApi);
base_access!(ClassifierModel, ClassifierApi);

impl AnyModel {
    pub fn model_type(&self) -> ModelType {
        match self {
            Self::Chat(_) => ModelType::Chat,
            Self::Image(_) => ModelType::Image,
            Self::Classifier(_) => ModelType::Classifier,
        }
    }
    pub fn id(&self) -> &str {
        match self {
            Self::Chat(m) => &m.id,
            Self::Image(m) => &m.id,
            Self::Classifier(m) => &m.id,
        }
    }
    pub fn provider(&self) -> &str {
        match self {
            Self::Chat(m) => &m.provider,
            Self::Image(m) => &m.provider,
            Self::Classifier(m) => &m.provider,
        }
    }
}

impl Model {
    pub fn supported_thinking_levels(&self) -> impl Iterator<Item = ThinkingLevel> + '_ {
        ThinkingLevel::ALL.into_iter().filter(|level| {
            if !self.reasoning {
                return *level == ThinkingLevel::Off;
            }
            match self
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.get(level))
            {
                Some(mapping) => mapping.is_some(),
                None => !matches!(level, ThinkingLevel::Xhigh | ThinkingLevel::Max),
            }
        })
    }

    pub fn clamp_thinking_level(&self, level: ThinkingLevel) -> ThinkingLevel {
        let mut below = ThinkingLevel::Off;
        for candidate in self.supported_thinking_levels() {
            if candidate as usize >= level as usize {
                return candidate;
            }
            below = candidate;
        }
        below
    }

    pub fn supports_images(&self) -> bool {
        self.input.contains(&ModelInputModality::Image)
    }

    pub(crate) fn compat(&self, key: &str) -> &Value {
        self.compat
            .as_ref()
            .and_then(|fields| fields.get(key))
            .unwrap_or(&Value::Null)
    }
}
