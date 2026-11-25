use serde_json::{Map, Value};

use crate::model::Model;

pub struct CompletionCompat<'m> {
    pub supports_store: bool,
    pub developer: bool,
    pub reasoning_effort: bool,
    pub usage: bool,
    pub finish_reason: bool,
    pub max_tokens_field: &'m str,
    pub tool_result_name: bool,
    pub assistant_after_tool: bool,
    pub non_null_assistant: bool,
    pub tools_with_history: bool,
    pub thinking_as_text: bool,
    pub assistant_reasoning_content: bool,
    pub thinking_format: &'m str,
    pub openrouter_routing: Option<&'m Value>,
    pub vercel_routing: Option<&'m Value>,
    pub chat_template_kwargs: Option<&'m Map<String, Value>>,
    pub chat_template_args: Option<&'m Map<String, Value>>,
    pub zai_tool_stream: bool,
    pub thinking_budget_field: Option<&'m str>,
    pub strict: bool,
    pub grammar: bool,
    pub mid_conversation_system: bool,
    pub mid_conversation_tools: bool,
    pub anthropic_cache: bool,
    pub send_affinity: bool,
    pub affinity_format: &'m str,
    pub long_cache: bool,
    pub prompt_key_mode: &'m str,
    pub vllm_priority: Option<&'m Value>,
}

pub fn detect_compat(model: &Model) -> CompletionCompat<'_> {
    let p = model.provider.as_str();
    let base = model.base_url.as_str();
    let zai = matches!(p, "zai" | "zai-coding-cn")
        || base.contains("api.z.ai")
        || base.contains("open.bigmodel.cn");
    let together =
        p == "together" || base.contains("api.together.ai") || base.contains("api.together.xyz");
    let moonshot = matches!(p, "moonshotai" | "moonshotai-cn") || base.contains("api.moonshot.");
    let openrouter = p == "openrouter" || base.contains("openrouter.ai");
    let cf_worker = p == "cloudflare-workers-ai" || base.contains("api.cloudflare.com");
    let cf_gateway = p == "cloudflare-ai-gateway" || base.contains("gateway.ai.cloudflare.com");
    let nvidia = p == "nvidia" || base.contains("integrate.api.nvidia.com");
    let ant = p == "ant-ling" || base.contains("api.ant-ling.com");
    let cerebras = p == "cerebras" || base.contains("cerebras.ai");
    let deepseek = p == "deepseek"
        || base
            .as_bytes()
            .windows(b"deepseek.com".len())
            .any(|w| w.eq_ignore_ascii_case(b"deepseek.com"));
    let grok = p == "xai" || base.contains("api.x.ai");
    let chutes = base.contains("chutes.ai");
    let non_standard = nvidia
        || cerebras
        || grok
        || together
        || chutes
        || deepseek
        || zai
        || moonshot
        || p == "opencode"
        || base.contains("opencode.ai")
        || cf_worker
        || cf_gateway
        || ant;
    let bool_ = |name, default| model.compat(name).as_bool().unwrap_or(default);
    let string = |name, default| model.compat(name).as_str().unwrap_or(default);
    let optional = |name| {
        model
            .compat
            .as_ref()
            .and_then(|m| m.get(name))
            .filter(|v| !v.is_null())
    };
    CompletionCompat {
        supports_store: bool_("supportsStore", !non_standard),
        developer: bool_(
            "supportsDeveloperRole",
            p == "openai" || base.contains("api.openai.com"),
        ),
        reasoning_effort: bool_(
            "supportsReasoningEffort",
            !(grok || zai || moonshot || together || cf_gateway || nvidia || ant),
        ),
        usage: bool_("supportsUsageInStreaming", true),
        finish_reason: bool_("supportsFinishReason", true),
        max_tokens_field: if string(
            "maxTokensField",
            if chutes || deepseek || moonshot || cf_gateway || together || nvidia || ant || zai {
                "max_tokens"
            } else {
                "max_completion_tokens"
            },
        ) == "max_tokens"
        {
            "max_tokens"
        } else {
            "max_completion_tokens"
        },
        tool_result_name: bool_("requiresToolResultName", false),
        assistant_after_tool: bool_("requiresAssistantAfterToolResult", false),
        non_null_assistant: bool_("requiresNonNullAssistantContent", false),
        tools_with_history: bool_("requiresToolsWithToolHistory", false),
        thinking_as_text: bool_("requiresThinkingAsText", false),
        assistant_reasoning_content: bool_("requiresReasoningContentOnAssistantMessages", deepseek),
        thinking_format: string(
            "thinkingFormat",
            if deepseek {
                "deepseek"
            } else if zai {
                "zai"
            } else if together {
                "together"
            } else if ant {
                "ant-ling"
            } else if openrouter {
                "openrouter"
            } else {
                "openai"
            },
        ),
        openrouter_routing: optional("openRouterRouting"),
        vercel_routing: optional("vercelGatewayRouting"),
        chat_template_kwargs: optional("chatTemplateKwargs").and_then(Value::as_object),
        chat_template_args: optional("chatTemplateArgs").and_then(Value::as_object),
        zai_tool_stream: bool_("zaiToolStream", false),
        thinking_budget_field: optional("thinkingTokenBudgetField")
            .and_then(Value::as_str)
            .or_else(|| {
                bool_("supportsThinkingTokenBudget", false).then_some("thinking_token_budget")
            }),
        strict: bool_("supportsStrictMode", false),
        grammar: bool_("supportsOpenAIGrammarTools", false),
        mid_conversation_system: bool_("supportsMidConvoSystemMessages", false),
        mid_conversation_tools: bool_("supportsMidConvoToolAdditions", false),
        anthropic_cache: string(
            "cacheControlFormat",
            if p == "openrouter" && model.id.starts_with("anthropic/") {
                "anthropic"
            } else {
                ""
            },
        ) == "anthropic",
        send_affinity: bool_("sendSessionAffinityHeaders", openrouter),
        affinity_format: string(
            "sessionAffinityFormat",
            if openrouter { "openrouter" } else { "openai" },
        ),
        long_cache: bool_(
            "supportsLongCacheRetention",
            !(together || cf_worker || cf_gateway || nvidia || ant),
        ),
        prompt_key_mode: string("promptCacheKeyMode", "auto"),
        vllm_priority: optional("vllmPriority"),
    }
}

pub fn short_hash(text: &str) -> String {
    let (mut a, mut b) = (0xdeadbeefu32, 0x41c6ce57u32);
    for ch in text.encode_utf16() {
        a = (a ^ u32::from(ch)).wrapping_mul(2654435761);
        b = (b ^ u32::from(ch)).wrapping_mul(1597334677);
    }
    a = (a ^ (a >> 16)).wrapping_mul(2246822507) ^ (b ^ (b >> 13)).wrapping_mul(3266489909);
    b = (b ^ (b >> 16)).wrapping_mul(2246822507) ^ (a ^ (a >> 13)).wrapping_mul(3266489909);
    fn base36(mut n: u32, out: &mut String) {
        let mut digits = [0; 7];
        let mut i = digits.len();
        loop {
            i -= 1;
            digits[i] = b"0123456789abcdefghijklmnopqrstuvwxyz"[(n % 36) as usize];
            n /= 36;
            if n == 0 {
                break;
            }
        }
        out.extend(digits[i..].iter().copied().map(char::from));
    }
    let mut out = String::with_capacity(14);
    base36(b, &mut out);
    base36(a, &mut out);
    out
}

pub fn normalize_completion_id(id: &str, model: &Model) -> Option<String> {
    if let Some((call, item)) = id.split_once('|') {
        let clean = |s: &str| {
            s.encode_utf16()
                .map(|unit| {
                    let c = char::from_u32(u32::from(unit)).unwrap_or('_');
                    if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                        c
                    } else {
                        '_'
                    }
                })
                .collect::<String>()
        };
        let call = clean(call);
        let item = clean(item);
        let total = call.len() + usize::from(!item.is_empty()) + item.len();
        return if total <= 40 {
            if item.is_empty() {
                Some(call)
            } else {
                Some(format!("{call}_{item}"))
            }
        } else {
            let hash = short_hash(id);
            let hash = &hash[..hash.len().min(8)];
            Some(format!(
                "{}_{hash}",
                &call[..call.len().min(39 - hash.len())]
            ))
        };
    }
    if model.provider == "openai" && id.encode_utf16().count() > 40 {
        return Some(String::from_utf16_lossy(
            &id.encode_utf16().take(40).collect::<Vec<_>>(),
        ));
    }
    None
}
