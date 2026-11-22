# Primary-Source Research: `p2` Extras (Discord, `p2-mac`, `voice-server`)

Covers Phase 3 milestones: `apps/p2/plugins/discord/` (part of Milestone 3.2 `pi-rs-p2`), Milestone 3.3 (`apps/p2-mac` -> `pi-rs-p2-mac`), and Milestone 3.4 (`apps/p2/voice-server` -> `pi-rs-voice-server`). Every claim cites upstream source.

## 1. Discord Plugin (`apps/p2/plugins/discord/bot.ts`, `fleet.ts`, `relay.ts`)

Upstream uses `discord.js` (`14.28.1`) strictly behind a narrow `DiscordPort` interface (`bot.ts#L26-L253`):
- **Gateway intents**: `Guilds`, `GuildMessages`, `MessageContent`; partials: `Channel`.
- **Events consumed**: `ClientReady`, `ShardReady` (triggers `{ kind: "ready" }` catch-up), `MessageCreate` (ignores self), `GuildCreate`, `GuildDelete`, `Error`.
- **REST operations used**:
  - `channels.fetch(id)` (text/thread metadata, `parentId`, `lastMessageId`, `guild.name`, `guild.premiumTier` for upload limit `10`/`50`/`100` MiB)
  - `channel.messages.fetch({ after, limit, cache: false })` sorted by BigInt snowflake ascending
  - `channel.send({ content?, files?, reply: { messageReference, failIfNotExists: false }, allowedMentions: { parse: ["users"] } })`
  - `channel.sendTyping()`
  - `guilds.fetch(guildId)` + `guild.channels.fetch()` (filter `GuildText` / `GuildAnnouncement`, sorted by `localeCompare`) + `guild.roles.fetch()` (exclude `@everyone` and `managed`, sorted)
  - `application.fetch()` for invite URL (`permissions = (1<<10)|(1<<11)|(1<<14)|(1<<15)|(1<<16)|(1<<38)`)
- **Exact behavioral traps**:
  - Refused token (`invalid token` / `TokenInvalid`) or disallowed intent (`disallowed intents`) must **not** schedule the 60s reconnect timer (`bot.ts#L89-L94,L222-L227`); transient reachability failures retry every `60_000 ms`.
  - Bot role mentions (`<@&botRoleId>`) rewrite to `<@botId>` and set `mentionsBot = true` (`bot.ts#L233-L246`).
- **Rust crate choice**: **`twilight-gateway` + `twilight-http` + `twilight-model` + `twilight-cache-inmemory`** (modular, minimal footprint, explicit shard/reconnect control matching `Bot`'s generation counter and retry policy; no slash-command framework bloat).

## 2. `p2-mac` Native macOS Shell (`apps/p2-mac/`)

Upstream (`Package.swift`, `README.md`, `Sources/P2Mac/*.swift`) is a ~3.5k LOC AppKit + `WKWebView` menu-bar agent (`LSUIElement`, macOS 14+) with one external dep (`Sparkle 2.10.1`):
- **Core responsibilities**:
  1. Menu bar `NSStatusItem` (`PiGlyph` template icon) + `NSMenu` (server status, server scanner for loopback `p2` processes via `/health` + `P2_DATA_DIR`, checkout selector, Radius sign-in/out, Appearance, Check for Updates, Quit).
  2. Child `p2` process supervisor (`ServerController.swift`: probes `/health` every 1.5s, login-shell env, `SIGTERM` then `SIGKILL` after 15s, restarts adopted dev servers via `<data>/dev.sock`, pre-upgrade `<data>.backups/<previous>` keeping 3 copies).
  3. Local owner sign-in (`LocalSignIn.swift`: reads `<data>/owner-key`, solves `/owner/local/challenge` -> `/owner/local/claim`, injects `HttpOnly` `p2_owner_<port>` cookie into per-brain `WKWebsiteDataStore`).
  4. Radius account (`RadiusAccount.swift`, `OAuthLoopback.swift`, `ViewCookie.swift`): shares `<P2_DATA_DIR>/radius.json` (mode `0600`), PKCE S256 system-browser loopback OAuth (`client = pi-gateway`, `scope = gateway offline_access`), polls `GET /v1/pi/servers` every 5s, renews remote brain view cookies every 6h via `POST /v1/pi/servers/{org}/{server}/open?path=`.
  5. Frameless `WKWebView` windows + native brain tab bar + `window.webkit.messageHandlers.piHost` bridge (`theme`, `set-theme`, `pi-host-theme` event, main-frame + own-origin enforcement) + `data-shell="macos"` / `--shell-inset-start` injection + title-bar drag hit-testing.
- **Rust crate choice**:
  - Cross-platform core (`server_config`, `server_controller`, `server_scan`, `dev_control`, `data_backup`, `local_sign_in`, `radius_account`, `oauth_loopback`, `view_cookie`, `brain_directory`) written in portable Rust (`tokio`, `reqwest`, `axum`, `serde_json`, `sha2`) — fully unit- and integration-testable on Linux `x86_64`.
  - macOS UI shell behind `#[cfg(target_os = "macos")]` using **`objc2` + `objc2-app-kit` + `objc2-web-kit` + `objc2-foundation`** (direct typed bindings to `NSStatusBar`, `NSWindow`, `WKWebView`, `WKWebsiteDataStore`, `WKScriptMessageHandlerWithReply` — needed because `wry` does not expose `WKWebsiteDataStore(forIdentifier:)` per-brain cookie isolation or custom AppKit titlebar `BrainTabBar` subviews cleanly).
  - Auto-updates: Rust Ed25519 appcast checker (`ed25519-dalek` + `reqwest`) replacing Sparkle for 100% Rust builds (or optional Sparkle FFI behind a Cargo feature).
  - Build/test policy per ADR-005: full logic/protocol test suite runs on Linux `x86_64`; `#[cfg(target_os = "macos")]` AppKit/WKWebView UI code compiles cleanly behind target cfg with live macOS verification deferred.

## 3. `voice-server` Speech Service (`apps/p2/voice-server/`)

Upstream (`server.py`, `engines.py`, `segmenter.py`, `supertonic_helper.py`, `Dockerfile`) is 1,075 LOC Python exposing an authenticated HTTP/WS CPU speech service on port `8000` (`PORT`):
- **Auth & routes**:
  - `Authorization: Bearer $VOICE_TOKEN` compared in constant time (`subtle::ConstantTimeEq`) on all routes except `GET /health`.
  - `GET /health` -> `{"ok": true}`
  - `GET /voices` -> `{"engines": [{"id", "name", "streaming", "sampleRate", "voices": [{"id", "name"}]}], "default": {"engine", "voice"}}` (voice IDs `<engine>-<voice>` matching `^[A-Za-z0-9_-]{1,32}$`; default voice listed first).
  - `POST /speak` -> `{"text", "engine"?, "voice"?, "speed"?}` (`text` trimmed `1..=2000` chars; `speed` `0.5..=2.0`) -> `audio/wav` 16-bit mono PCM + headers `X-Synthesis-Ms`, `X-Voice`.
  - `WS /speak/stream` (max msg 64 KiB, heartbeat 20s): text `{"type":"speak","id","text",...}` (`id` `^[A-Za-z0-9_-]{1,64}$`, unique while pending) and `{"type":"cancel","id"?}`; replies `ready`, `start`, binary `[1B id_len][id ASCII][PCM16LE]`, `done` (`cancelled`, `firstChunkMs`, `ms`, `audioMs`), or `error`. Sequential per-socket FIFO queue.
  - `WS /listen` (max msg 1 MiB, heartbeat 20s): binary 16 kHz mono PCM16LE in, `{"type":"reset"}` text in; replies `ready` (`sampleRate: 16000`), `speech_start`, `speech_end`, `final` (`text`, `ms`) delivered strictly in utterance order.
- **Audio & ML backends**:
  - **Web server**: `axum` (`ws` feature) + `tokio` dedicated blocking pools (2 STT threads, 4 TTS threads).
  - **Segmenter (`segmenter.py`)**: 512-sample Silero VAD windows (`VOICE_VAD_THRESHOLD=0.5`, `VOICE_VAD_SILENCE=1.2`, `min_speech=0.25`, `max_speech=30`) + rolling `preroll_ms` (`750 ms`) history buffer extending detected onset without duplicating samples or crossing `previous_end`.
  - **ONNX Runtime (`ort` 2.0)**: `CPUExecutionProvider`, `intra_op_num_threads = VOICE_TTS_THREADS (8)`, `inter_op_num_threads = 1`, `session.intra_op.allow_spinning = "0"`, `session.inter_op.allow_spinning = "0"`:
    - **Silero VAD**: `silero_vad.onnx` (stateful LSTM + 512-sample windows)
    - **Parakeet TDT 0.6B v3 int8**: `encoder.int8.onnx`, `decoder.int8.onnx`, `joiner.int8.onnx`, `tokens.txt` (NeMo log-mel filterbank via `rustfft` + TDT greedy transducer decoder)
    - **Supertonic 3**: `duration_predictor.onnx`, `text_encoder.onnx`, `vector_estimator.onnx`, `vocoder.onnx`, `tts.json`, `unicode_indexer.json`, `voice_styles/*.json` + full port of `UnicodeProcessor` (NFKD, emoji strip, punctuation/abbreviation `chunk_text` using `fancy-regex` for lookbehinds) and Euler flow loop (`VOICE_TTS_STEPS=5`, `default_speed=1.05`, `0.25s` inter-sentence gap)
    - **Kokoro 82M v1.0**: `kokoro-v1.0.onnx` + `voices-v1.0.bin` + English phonemizer
  - **Pocket TTS (Kyutai) caveat**: Upstream uses PyTorch (`pocket-tts`) + `.safetensors` KV-cache states (`engines.py#L146-L166`). In 100% Rust (ADR-002), abstract behind a `TtsEngine` trait and either run Pocket TTS via `candle-core` + `safetensors` (matching `kyutai/pocket-tts` architecture) or ONNX-exported weights; all HTTP/WS/WAV/PCM/segmenter/chunker logic must be 100% offline-testable via mock engines plus synthetic ONNX smoke tests without requiring gigabytes of downloaded HuggingFace weights in CI.
