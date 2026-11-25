# pi-voice-server

Local speech service for p2. It binds `0.0.0.0:$PORT` and uses local model files; it does not download anything at startup.

| Route | Contract |
| --- | --- |
| `GET /health` | Public `{"ok":true}`. |
| `GET /voices` | Engines, voice IDs (`<engine>-<voice>`), sample rates, and the effective default. |
| `POST /speak` | JSON `{text, engine?, voice?, speed?}`; returns mono PCM16LE WAV with `X-Voice` and `X-Synthesis-Ms`. Text is limited to 2,000 Unicode characters after trimming; request body is limited to 64 KiB. |
| `GET /speak/stream` | WebSocket: `ready`, then FIFO `speak`/`cancel` requests. Audio frames are `[id length byte][ASCII id][PCM16LE]`; each accepted job ends with `done` or `error`. Incoming messages are limited to 64 KiB. |
| `GET /listen` | WebSocket: send 16 kHz mono PCM16LE. Receives `ready`, `speech_start`, `speech_end`, and ordered `final` transcripts. `{"type":"reset"}` clears the current segment. Incoming messages are limited to 1 MiB. |

Every route except `GET /health` requires `Authorization: Bearer $VOICE_TOKEN`, including WebSocket upgrades. Both WebSockets use a 20-second heartbeat.

| Environment | Default | Effect |
| --- | --- | --- |
| `PORT` | `8000` | TCP port. |
| `VOICE_TOKEN` | required | Shared bearer token. |
| `VOICE_MODELS` | `/models` | Model directory. |
| `VOICE_ENGINES` | `supertonic,pocket,kokoro` | Requested engines, in catalog order. See Pocket status below. |
| `VOICE_DEFAULT_ENGINE`, `VOICE_DEFAULT` | `supertonic`, engine's default | `VOICE_DEFAULT` is an unprefixed name, e.g. `F1` or `af_heart`. An unavailable engine falls back to the first loaded engine. |
| `VOICE_TTS_THREADS`, `VOICE_TTS_STEPS` | `8`, `5` | ONNX intra-op threads per TTS session; diffusion iterations for Supertonic. The service uses a separate four-worker TTS pool and two-worker STT pool. |
| `VOICE_VAD_THRESHOLD`, `VOICE_VAD_SILENCE`, `VOICE_VAD_PREROLL_MS` | `0.5`, `1.2`, `750` | Silero threshold, trailing silence in seconds, and retained lead-in in milliseconds. |
| `ORT_DYLIB_PATH` | platform library name | Path to a CPU ONNX Runtime shared library exposing the 1.24 API or later. |
| `PHONEMIZER_ESPEAK_LIBRARY` | platform library name | Optional path to eSpeak NG's shared library (Kokoro also needs the eSpeak NG voice data). |

Model layout matches the [upstream Dockerfile](../../../apps/p2/voice-server/Dockerfile):

- `silero_vad.onnx`: sherpa-onnx's Silero V4 export.
- `parakeet/{encoder.int8.onnx,decoder.int8.onnx,joiner.int8.onnx,tokens.txt}`: sherpa-onnx's NeMo Parakeet TDT 0.6B v3 int8 export.
- `supertonic3/onnx/{duration_predictor.onnx,text_encoder.onnx,vector_estimator.onnx,vocoder.onnx,tts.json,unicode_indexer.json}` and `supertonic3/voice_styles/{F1..F5,M1..M5}.json`.
- `kokoro/{kokoro-v1.0.onnx,voices-v1.0.bin}`: Kokoro 82M v1.0.
- `pocket/{pocket_tts.onnx,pocket.json,<voice>.safetensors}`: Pocket TTS ONNX export and prompt-state `.safetensors` tensors (`F32`, `F16`, or `BF16`).

**Pocket:** `Config::load` loads `PocketOnnx` automatically whenever `<models>/pocket/<onnx_model>` (default `pocket_tts.onnx`) is present alongside `<voice>.safetensors` prompt states, and skips `pocket` with a diagnostic only when the ONNX model file is not installed. Optional `pocket.json` configures `sample_rate` (default `24000`), `chunk_samples` (default `4800`), `onnx_model`, `vocab`, `token_encoding` (`auto` | `codepoints` | `utf8_bytes` | `vocab`), `tokens_input`, `state_input`, `state_tensor`, `speed_input`, `steps_input`, `default_speed`, and `audio_output`. This crate does not shell out to Python.

Build and run from this directory:

```sh
cargo build --locked --release -j 2
VOICE_TOKEN=local-secret VOICE_MODELS=/models VOICE_ENGINES=supertonic,kokoro \
  ORT_DYLIB_PATH=/path/to/libonnxruntime.so ./target/release/pi-voice-server
```

## Verification

The transport tests exercise real loopback HTTP and WebSocket connections with deterministic inference. The suite does not need model weights, ONNX Runtime, or eSpeak NG. With the Cargo dependencies cached:

```sh
cargo fmt --check
cargo clippy --offline --locked -j 2 --all-targets -- -D warnings
cargo test --offline --locked -j 2
```

To also run the real ONNX Runtime end-to-end tests:

```sh
ORT_DYLIB_PATH=/path/to/libonnxruntime.so cargo test --offline --locked -j 2 -- --include-ignored
```

Verified: formatting and Clippy are clean, and all 17 offline tests pass. Removing the previous-utterance clamp made the pre-roll regression fail; restoring it made the suite pass.

With the upstream weights and ONNX Runtime 1.28.2, the native executable returned non-silent PCM WAVs from Supertonic (44.1 kHz) and Kokoro (24 kHz). `/listen` detected and transcribed generated speech and returned the same transcript as sherpa-onnx 1.13.8 for the upstream Parakeet English reference clip.
