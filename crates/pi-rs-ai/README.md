# pi-rs-ai

Native Rust model catalog, request routing, and provider adapters. The crate is a standalone Cargo workspace; it does not load Node, SDK JavaScript, or the upstream model tree at runtime.

- `Models::builtin(ModelsOptions::default())` registers the embedded providers. Use `Models::from_catalog` with `Catalog::from_hydrated_dir` to load a local catalog snapshot instead.
- `Models::stream` and `Models::stream_simple` normalize the transcript, resolve provider auth, and return an `AssistantMessageEventStream`. Consume events with `StreamExt::next`, or await `result()` without draining the events.
- `ModelsOptions` accepts an application-owned `CredentialStore`, `ModelsStore`, and `AuthContext`. `FileCredentialStore` writes canonical `auth.json` entries under a file lock, using an atomic replacement and mode `0600` on Unix.
- `ProviderRequestOptions::client` accepts an explicitly supplied HTTP client. Direct provider modules accept already-resolved credentials and a `TranscriptContext`. `compat` exposes the old catalog and API-dispatch surface separately.
- `pi-ai` supports `help`, `list`, and `login [provider]`. `login` writes `auth.json` in the working directory. Offline verification never invokes a live login.

## Verification

From this directory:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The integration suite uses loopback HTTP, SSE, WebSocket, and AWS EventStream fixtures. OAuth endpoints are remapped to loopback during tests. Credentials and model histories are synthetic; no provider account or network catalog is needed.

The sibling `pi-rs-ai-cache-warming` and `pi-rs-ai-image-models` bridge crates are differential proofs, not runtime dependencies. Their recorded cases cover the corresponding upstream pure-data APIs.

### Milestone 1.6 gates

| Gate | Result | Evidence |
| --- | --- | --- |
| Cache-warming leaf | Pass | 713/713 recorded cases; bridge-inclusive benchmark 0.09× upstream |
| Image-models leaf | Pass | 172/172 recorded cases; bridge-inclusive benchmark 0.01× upstream |
| Native AI crate | Pass | `cargo fmt --check`, Clippy with `-D warnings`, 39/39 offline tests |

The offline suite covers all 10 chat transports, 1 image transport, 4 classifier transports, and 9 OAuth providers. The Bedrock regression test failed when the old frame-prelude handling was restored and passed with the fix.
