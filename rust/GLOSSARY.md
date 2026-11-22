# `pi-rs` Domain Glossary

Maps upstream TypeScript/Swift/Python domain terms to canonical Rust names so every crate and unit names the same concept identically. Expand as units are ported.

## Workspace & Crate Map

| Upstream Package / App | Rust Crate (`Cargo.toml`) | Rust Lib / Module | Binary |
| --- | --- | --- | --- |
| `@earendil-works/pi-schema` | `pi-rs-schema` | `pi_rs_schema` | - |
| `@earendil-works/pi-telemetry` | `pi-rs-telemetry` | `pi_rs_telemetry` | - |
| `@earendil-works/pi-tui` | `pi-rs-tui` | `pi_rs_tui` | - |
| `@earendil-works/pi-mcp` | `pi-rs-mcp` | `pi_rs_mcp` | - |
| `@earendil-works/pi-codemode` | `pi-rs-codemode` | `pi_rs_codemode` | - |
| `@earendil-works/pi-ai` | `pi-rs-ai` | `pi_rs_ai` | `pi-ai` |
| `@earendil-works/pi-agent-core` | `pi-rs-agent` | `pi_rs_agent` | - |
| `@earendil-works/pi-coding-agent` | `pi-rs-coding-agent` | `pi_rs_coding_agent` | `pi` |
| `@earendil-works/chord` | `pi-rs-chord` | `pi_rs_chord` | - |
| `@earendil-works/pi-durable` | `pi-rs-durable` | `pi_rs_durable` | - |
| `@earendil-works/pi-env` (+ `daemon/`) | `pi-rs-env` | `pi_rs_env` | `pi-env` |
| `@earendil-works/pi-evals` | `pi-rs-evals` | `pi_rs_evals` | - |
| `@earendil-works/p3` | `pi-rs-p3` | `pi_rs_p3` | `p3` |
| `apps/p2` | `pi-rs-p2` | `pi_rs_p2` | `p2` |
| `apps/p2-mac` | `pi-rs-p2-mac` | `pi_rs_p2_mac` | `p2-mac` |
| `apps/p2/voice-server` | `pi-rs-voice-server` | `pi_rs_voice_server` | `pi-voice-server` |

## Core Domain Terms

### Schema (`pi-rs-schema`)
| Upstream Term | Meaning | Rust Name |
| --- | --- | --- |
| `JsonSchema` | Draft 2020-12 or draft 7 JSON Schema value | `JsonSchema` (`serde_json::Value` wrapper/enum) |
| `Validator` | Compiled non-eval schema validator | `Validator` |
| `CoercingValidator` | Validator that coerces inputs before checking | `CoercingValidator` |
| `ValidationError` | Path + message + keyword failure | `ValidationError` |
| `ParseResult<T>` | Validation success or list of errors | `Result<T, Vec<ValidationError>>` |

### Telemetry (`pi-rs-telemetry`)
| Upstream Term | Meaning | Rust Name |
| --- | --- | --- |
| `TelemetryContext` | Root/parent span factory carrying optional metrics and propagation | `TelemetryContext` trait |
| `TelemetrySpan` | Active span with attributes, events, links, status | `TelemetrySpan` trait |
| `TelemetryCarrier` | Opaque string map for cross-boundary trace propagation | `TelemetryCarrier` (`IndexMap<String, String>`) |
| `StoredSpan` / `StoredTrace` | Settled immutable span/trace snapshots for queries & rendering | `StoredSpan` / `StoredTrace` |
| `TelemetryStore` | Storage-independent query interface (`findTraces`, `getTrace`, `findMetrics`) | `TelemetryStore` trait |

### AI (`pi-rs-ai`)
| Upstream Term | Meaning | Rust Name |
| --- | --- | --- |
| `Api` (`KnownApi`) | Wire protocol (`anthropic-messages`, `openai-responses`, `bedrock-converse-stream`, etc.) | `Api` enum (`#[non_exhaustive]` + `Custom(String)`) |
| `ProviderId` (`KnownProvider`) | Model service provider (`anthropic`, `openai`, `openrouter`, etc.) | `ProviderId` |
| `Model` | Catalog entry describing capabilities, context window, cost, thinking levels, compat flags | `Model` |
| `ThinkingLevel` | `off` / `minimal` / `low` / `medium` / `high` / `xhigh` / `max` | `ThinkingLevel` enum |
| `TranscriptContext` | Normalized conversation messages (system prompt & tools carried in `SystemMessage`) | `TranscriptContext` |
| `AssistantMessageEventStream` | Async stream of incremental deltas culminating in final `AssistantMessage` | `AssistantMessageEventStream` |
| `ClientIdentity` | Attribution headers sent to providers | `ClientIdentity` |

### Agent (`pi-rs-agent`)
| Upstream Term | Meaning | Rust Name |
| --- | --- | --- |
| `Agent` | Stateful conversation controller with queues, listeners, and hooks | `Agent` |
| `agentLoop` | Stateless turn runner (`prepareRequest` -> stream -> tools -> `finishTurn`) | `agent_loop` |
| `AgentMessage` | Extensible message enum (LLM roles + custom app messages) | `AgentMessage` |
| `AgentTool` | Executable tool with JSON Schema params, `executionMode`, streaming updates | `AgentTool` trait |
| `Steering` / `FollowUp` | Queued user messages injected mid-run vs after natural stop (`one-at-a-time` / `all`) | `QueueMode`, `steer()`, `follow_up()` |
| `ToolExecutionMode` | `parallel` (preflight seq, run concurrent, emit in source order) vs `sequential` | `ToolExecutionMode` |

### TUI (`pi-rs-tui`)
| Upstream Term | Meaning | Rust Name |
| --- | --- | --- |
| `TUI` | Shared terminal UI interface (children, focus, overlays, input, lifecycle) | `Tui` trait |
| `TuiMainScreen` | Differential renderer preserving terminal scrollback | `TuiMainScreen` |
| `TuiAltScreen` | Fixed-viewport alternate-screen renderer with `VStack`/`HStack`/`ScrollView`/search | `TuiAltScreen` |
| `Component` | Renderable UI node (`render(width) -> Vec<String>`, `handle_input`) | `Component` trait |
| `Overlay` / `OverlayHandle` | Layered floating modal/dialog positioned by anchor, percentage, or row/col | `OverlayOptions`, `OverlayHandle` |
| `Color` | Indexed ANSI, sRGB, OKLCH, or OKHSL color value | `Color` enum |

### MCP (`pi-rs-mcp`) & Codemode (`pi-rs-codemode`)
| Upstream Term | Meaning | Rust Name |
| --- | --- | --- |
| `McpClient` | Transport-neutral JSON-RPC MCP client (tools, resources, prompts, OAuth) | `McpClient` |
| `StdioTransport` / `StreamableHttpTransport` / `PipeTransport` | MCP transport implementations | `StdioTransport`, `StreamableHttpTransport`, `PipeTransport` |
| `Codemode` | Sandboxed async JS execution (`boa_engine`) calling injected `tools`/`models`/`store` | `CodemodeRuntime` |

### Coding Agent (`pi-rs-coding-agent`)
| Upstream Term | Meaning | Rust Name |
| --- | --- | --- |
| `AgentSession` | Core session lifecycle shared across interactive, print, JSON, and RPC modes | `AgentSession` |
| `SessionManager` | JSONL v3 tree persistence (`id`/`parentId`), branching, compaction, context edits | `SessionManager` |
| `CompactionEntry` | Summary + `firstKeptEntryId` + replayed `systemMessage` checkpoint | `CompactionEntry` |
| `ContextEditEntry` | Append-only branch-relative omission or replacement of an earlier message | `ContextEditEntry` |
| `ExtensionAPI` / `ExtensionRunner` | Extension host registering tools, hooks, commands, shortcuts, providers, UI | `ExtensionApi`, `ExtensionRunner` (Rust traits + WASM/WIT host) |
| `ResourceLoader` | Discovers extensions, skills, prompt templates, themes, and context files | `ResourceLoader` |
| `ProjectTrustStore` | Per-project trust decision gating project-local resources | `ProjectTrustStore` |
| `RpcMode` / `RpcClient` | Strict LF-framed JSONL stdin/stdout control protocol | `RpcServer`, `RpcClient` |

### Platform (`pi-rs-chord`, `pi-rs-durable`, `pi-rs-env`)
| Upstream Term | Meaning | Rust Name |
| --- | --- | --- |
| `Contract` (`local` / `remote`) | Versioned typed object interface (name, params schema, members) | `Contract` |
| `Facet` | A plugin's declarations for one host kind (served types + registry entries) | `Facet` |
| `Host` | Runtime activating facets, holding references, running jobs, and handling reloads | `Host` |
| `Principal` | Caller identity stamped by links; scopes object instances by principal key | `Principal`, `PrincipalKey` |
| `Document` (`defineState`) | Transactional JSON value owned by an object and persisted in Chord's store | `Document` |
| `Harness` (Durable) | Durable conversation/task/document runtime committing every turn atomically | `Harness` |
| `Submission` (Durable) | Durably admitted user input or entry write (`done` / `unanswered`) | `Submission` |
| `ExecutionEnv` (Env) | Client handle to a local or SSH-deployed `pi-env` Rust daemon | `ExecutionEnv` |
