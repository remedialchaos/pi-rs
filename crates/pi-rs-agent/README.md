# pi-rs-agent

Native Rust port of `@earendil-works/pi-agent-core` (`packages/agent`). The crate orchestrates turns and tools; `pi-rs-ai` owns providers, transcripts and assistant streams, and `pi-rs-schema` owns argument coercion and validation.

- `Agent` manages the conversation, steering and follow-up queues, cancellation, and awaited event subscribers. Use `models_stream_fn` with a `pi-rs-ai::Models` registry, `proxy_stream_fn` for `/api/stream`, or provide a `StreamFn`. A host may install a fallback with `set_default_stream_fn`.
- `agent_loop` and `agent_loop_continue` return an event stream and a separately awaitable result. The `run_agent_loop*` variants await an event sink directly. Custom messages stay in host history unless converted for the provider.
- `run_tool_call` applies the same schema and hooks to nested calls. In parallel mode, `tool_execution_end` follows completion order; tool-result messages remain in assistant source order. Steering never interrupts a tool batch; aborted calls still get error results.
- The system prompt is replayed from transcript messages. `append_system_prompt` adds an update; `reset` retains the replayed system and tool baseline. `wait_for_idle` includes asynchronous `agent_end` listeners. Call `Subscription::unsubscribe` explicitly to remove a listener.

## Verification

Run `cargo fmt --all -- --check`, `cargo clippy -p pi-rs-agent --all-targets --locked -- -D warnings`, and `cargo test -p pi-rs-agent --locked` from the repository root. The native suite uses scripted streams, the real `pi-rs-ai` faux provider, and a loopback Axum SSE server; no API keys or external services are needed. It covers exact event and transcript ordering, asynchronous settlement, queues, argument coercion and hooks, cancellation, terminal failures, and proxy framing/partial tool arguments. TypeScript declaration merging and TypeBox static inference are not Rust interfaces; custom messages and runtime JSON Schemas cover the corresponding runtime contracts.

Historically at fork baseline `780bf31fc`, the unmodified upstream suite passed 96/96.

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy -p pi-rs-agent --all-targets --locked -- -D warnings` | Passed offline |
| `cargo test -p pi-rs-agent --locked` | 32/32 passed offline in 0.93 s (test execution only) |

Reversing parallel tool-result persistence made the ordering regression fail; restoring source order made it pass.
