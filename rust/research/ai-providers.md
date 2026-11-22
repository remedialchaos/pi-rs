# Primary-Source Research: `pi-rs-ai` Wire APIs, Auth & Dependencies

Covers Milestone 1.6 (`packages/ai` -> `pi-rs-ai`). Every claim cites upstream source (`packages/ai/`) or `docs.rs`.

## 1. Wire API Matrix & Crate Replacements

Upstream `packages/ai/src/types.ts#L66-L90` defines **10 chat APIs**, **1 image API**, and **4 classifier APIs**:

| Category | Wire API IDs | Upstream SDK / Transport | Rust Replacement (`docs.rs`) |
| --- | --- | --- | --- |
| Anthropic | `anthropic-messages` | `@anthropic-ai/sdk` (HTTP POST + custom SSE parser in `anthropic-messages.ts#L767-L806`) | Direct **`reqwest`** (`stream`) + custom SSE line decoder (`\r`, `\n`, `\r\n`) |
| OpenAI / Azure | `openai-completions`, `openai-responses`, `azure-openai-responses`, `openrouter-images` | `openai` (`OpenAI`, `AzureOpenAI`) | Direct **`reqwest`** + SSE parser (`eventsource-stream` `0.2.3` or shared in-house SSE decoder) |
| Codex | `openai-codex-responses` | Raw `fetch` (+ optional `zstd` level 3 body) + `WebSocket` | **`reqwest`** + **`zstd`** + **`tokio-tungstenite`** (`0.30.x`) |
| Google | `google-generative-ai`, `google-vertex` | `@google/genai` (`:streamGenerateContent?alt=sse`) | Direct **`reqwest`** + Google ADC/service-account JWT/OAuth token provider + SSE parser |
| AWS Bedrock | `bedrock-converse-stream` | `@aws-sdk/client-bedrock-runtime` + `credential-provider-node` + `node-http-handler` | **`aws-config`** (`1.13.x`) + **`aws-sigv4`** (`1.6.x`) + **`aws-smithy-eventstream`** (`0.61.x`) over **`reqwest`** (or **`aws-sdk-bedrockruntime`** `1.156.x`; direct `aws-config` + `aws-sigv4` + `aws-smithy-eventstream` + `reqwest` gives full control over custom pre-SigV4 headers, raw status/headers `onResponse`, HTTP/1.1 proxy forcing, `AWS_BEDROCK_SKIP_AUTH`, and `httpBearerAuth`!) |
| Zero-SDK APIs | `mistral-conversations`, `pi-messages`, `typesafe-system-one`, `cloudflare-workers-ai-system-one`, `llama-cpp-classify`, `openai-decisions` | Raw `fetch` (HTTP JSON / SSE) | Direct **`reqwest`** + shared SSE parser |
| Proxies | `http-proxy-agent`, `https-proxy-agent`, `node-http-proxy.ts` | Custom env resolution + proxy agents | **`reqwest::Proxy`** driven by ported `resolve_http_proxy_url_for_target` (honoring provider-scoped `env` overrides, case-insensitive `http_proxy`/`https_proxy`/`all_proxy`/`no_proxy`, IPv6 brackets, `*.domain`/`.domain`, port-specific `no_proxy`, rejecting non-`http:`/`https:` schemes) |
| Streaming JSON | `partial-json` (`0.1.7`, fallback only in `json-parse.ts#L129-L149`) + in-house `streaming-json.ts` | In-house `StreamingJsonParser` + `repairJson()` | Port `streaming-json.ts` + `json-parse.ts` directly in pure Rust (zero external partial-json crate needed) |

*(Note on `packages/ai` false blockers: all 4 `blocked` flags in `rust-anything analyze` were JS lazy `import()` shims for bundler splitting — zero `eval`/`new Function`. In Rust, all APIs and providers are normal compiled modules.)*

## 2. Transport & Streaming Details

- **HTTP SSE**: Seven chat APIs (`anthropic-messages`, `openai-completions`, `openai-responses`, `azure-openai-responses`, `google-generative-ai`, `google-vertex`, `mistral-conversations`, `pi-messages`) plus Codex fallback. Each adapter has specific framing nuances:
  - `anthropic-messages`: requires `message_stop` whenever `message_start` was seen (`#L803-L805`); checks `event: error`.
  - `openai-codex-responses`: optional `Content-Encoding: zstd` (level 3) request body (`#L223-L237`); EOF-flushes residual SSE frame and ignores `[DONE]` (`#L847-L907`).
  - `mistral-conversations`: camelCase -> snake_case wire remapping (`#L378-L443`); splits SSE on `\r\n\r\n`, `\n\n`, `\r\r`.
- **Codex WebSockets (`openai-codex-responses.ts`)**:
  - Default transport (`auto` | `websocket` | `websocket-cached`); scheme `https->wss`, `http->ws`.
  - Handshake headers: `OpenAI-Beta: responses_websockets=2026-02-06`, `x-client-request-id`, `session-id`.
  - Connection pool keyed by `(sessionId, accountId)`: 5-minute idle TTL (`#L914`), 45-minute max age (`#L917`).
  - Incremental delta continuation (`previous_response_id` + delta `input`) because Codex enforces `store: false`; retries once on `previous_response_not_found` and pre-start `websocket_connection_limit_reached`; pre-start WS failure permanently records `sessionId` in `websocketSseFallbackSessions` and falls back to HTTP SSE (`#L321-L398`).
- **AWS Bedrock (`bedrock-converse-stream.ts`)**:
  - Endpoint `/model/{modelId}/converse-stream` returning `application/vnd.amazon.eventstream` binary frames (`messageStart`, `contentBlockStart`, `contentBlockDelta`, `contentBlockStop`, `messageStop`, `metadata`, plus inline stream exceptions).
  - Auth modes: (a) SigV4 via explicit env keys (`AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`/`AWS_SESSION_TOKEN`) or default credential chain (profiles/SSO/IMDS/ECS/web-identity), (b) dummy creds when `AWS_BEDROCK_SKIP_AUTH=1`, (c) `Authorization: Bearer` when `AWS_BEARER_TOKEN_BEDROCK` / `apiKey` / `bearerToken` is set.
  - Forces HTTP/1.1 when proxy is active or `AWS_BEDROCK_FORCE_HTTP1=1`.
- **Non-streaming JSON APIs**: `openrouter-images` (`modalities`), `typesafe-system-one` (`bool <-> noul`, `TypeSafe-Danger-Zone-Images`), `cloudflare-workers-ai-system-one`, `llama-cpp-classify` (`/tokenize`, `/apply-template`, `/completion` with escalating `n_probs` `[max(256, 16*labels), 4096, 32768]`), `openai-decisions` (explicitly does NOT retry HTTP 504).

## 3. Credential Resolution & 9 OAuth Flows (`packages/ai/src/auth/`)

- **Resolution precedence (`auth/resolve.ts#L33-L93`)**:
  1. Explicit `overrides.apiKey`.
  2. Stored `CredentialStore` entry (owns provider; **never** falls back to ambient env on failed refresh or type mismatch).
  3. Ambient sources (env vars, `~/.aws/*`, `~/.config/gcloud/application_default_credentials.json`).
- **Double-checked locked OAuth refresh (`auth/resolve.ts#L102-L191`)**:
  - Minimum remaining validity: **5 minutes** (`300_000 ms`).
  - Re-checks `needsRefresh` inside `CredentialStore::modify(provider_id, ...)` to prevent concurrent double-refresh of rotated refresh tokens.
  - Once inside the lock, ignores caller cancellation and runs with a fixed **15s timeout** so an abort cannot drop a newly rotated refresh token.
- **9 OAuth Providers (`packages/ai/src/auth/oauth/`)**:
  1. `anthropic`: PKCE (`state = verifier`), `127.0.0.1:53692/callback` (ephemeral fallback) raced vs manual paste, or headless `code#state`. Token prefix `sk-ant-oat` switches to `Authorization: Bearer`, `claude-cli` UA, `x-app: cli`, OAuth/Claude-Code betas, identity system block, and tool-name casing remap (`anthropic-messages.ts#L108-L145,L1254-L1312`).
  2. `openai-codex`: PKCE on `127.0.0.1:1455/auth/callback` or custom JSON device flow; extracts `chatgpt_account_id` from JWT claim `https://api.openai.com/auth`.
  3. `openai-chatgpt`: Dynamic client registration, strict `127.0.0.1:1455/auth/callback` (fails on `EADDRINUSE`, closes all keep-alive sockets), requires `client_id` + `id_token`.
  4. `github-copilot`: RFC 8628 device flow (github.com / GHE) -> `/copilot_internal/v2/token` -> parses `proxy-ep=` for dynamic `baseUrl`.
  5. `openrouter`: PKCE with `/oauth/callback/<uuid>` -> permanent key (`refresh` no-op).
  6. `kimi-coding`: RFC 8628 device flow, 3x refresh retry on 429/5xx.
  7. `meta`: RFC 8628 device flow -> 24h Model API key exchange at `api.meta.ai/muse-code/key`.
  8. `xai`: RFC 8628 device flow, tolerates omitted `refresh_token` on refresh.
  9. `radius`: Discovers `/v1/oauth`, supports PKCE (`127.0.0.1:1456/oauth/callback`) or device flow.

## 4. Critical Semantic Traps

1. **Retries & Abort (`utils/provider-retry.ts#L24-L128`)**: Disable all client SDK retries; implement unified `retry_provider_request` retrying `408, 409, 429, 5xx` and transport errors unless `x-should-retry: false`, respecting `retry-after-ms` then `retry-after` (seconds or HTTP-date). If server delay exceeds `max_retry_delay_ms` (default `60_000 ms`, `0` disables), fail immediately rather than sleeping.
2. **Header timeout vs stream timeout**: `timeoutMs` caps **waiting for HTTP response headers only**, never the entire streaming response duration (thinking streams can last minutes).
3. **Streaming JSON parser & repair (`utils/streaming-json.ts`, `json-parse.ts`)**: Credit-throttled open-path snapshots; tolerates missing colons/commas, raw control chars, invalid escapes (`\d`), holds back incomplete `\uXXXX`; `repairJson` escapes raw `0x00..0x1f` and doubles invalid backslashes; non-object or irrecoverable JSON yields `arguments = {}` + `argumentsError`. Grammar (`custom`) tools synthesize JSON deltas (`constrained-sampling.ts#L185-L219`).
4. **Cross-provider message transforms (`api/transform-messages.ts#L62-L256`)**:
   - Strip images on non-vision models; split images out of error `tool_result` blocks for Anthropic and Claude-on-Bedrock (`splitErrorToolResultImages`).
   - Drop cross-provider/model `thinking` blocks and strip `textSignature`/`thoughtSignature`; skip errored/aborted assistant messages.
   - Insert synthetic `"No result provided"` (`isError: true`) tool results for orphaned tool calls while holding back mid-call `system` messages.
   - Per-API tool call ID normalization: Anthropic/Google-v3+ `[^a-zA-Z0-9_-] -> _` max 64; OpenAI Completions `callId|itemId` -> 40 chars + `shortHash`; OpenAI Responses preserves `callId|fc_*`/`ctc_*`; Mistral maps to exact 9 alphanumeric chars with collision-checked `shortHash`.
5. **Provider-specific wire quirks**:
   - **OpenAI Completions**: `detectCompat` (`#L1594-L1694`) controls `max_tokens` vs `max_completion_tokens`, `developer` vs `system`, 10 `thinkingFormat` modes, `requiresAssistantAfterToolResult`, `requiresReasoningContentOnAssistantMessages`; reasoning priority `reasoning_content > reasoning > reasoning_text`; `input = prompt_tokens - cacheRead - cacheWrite`.
   - **OpenAI Responses / Azure / Codex**: Azure backfills `encrypted_content` from `response.completed.response.output`; Codex forbids `store: true` and `instructions` (puts system prompt in leading `developer` item).
   - **Anthropic**: Thread continuation (`previous_message_id` + prefix hash cache) with automatic 400 fallback; preserve `input_tokens` from `message_start` when `message_delta` omits it; Opus 4.7+ `output_config.effort` + `thinking.block_binding`.
   - **Bedrock**: Region precedence `ARN > option/env > endpoint > us-east-1`; empty text blocks -> `"<empty>"`; GovCloud omits `thinking.display`/`block_binding`; non-Claude omits reasoning signature; raw `redactedContent` bytes base64-encoded into `thinkingSignature`.
   - **Google / Vertex**: `thoughtSignature` may attach to any part and must survive deltas; rewrite JSON Schema `const` to single-value `enum`; `output = candidatesTokenCount + thoughtsTokenCount`.
   - **OpenRouter**: Provider-reported `cost` (+ `upstream_inference_cost` for BYOK) overrides `usage.cost.total` with `totalFromProvider = true`.
   - **Simple options clamping (`api/simple-options.ts`)**: `clampMaxTokensToContext` and `adjustMaxTokensForThinking` (`MIN_ANSWER_TOKENS = 1024`).
