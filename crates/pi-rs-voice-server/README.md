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
| `VOICE_POCKET_THREADS` | `2` | ONNX intra-op threads of the Pocket TTS session, like upstream's Pocket engine. |
| `VOICE_POCKET_MODEL` | unset | Path to the Pocket TTS ONNX weights, else the file in `<models>/pocket`. |
| `VOICE_VAD_THRESHOLD`, `VOICE_VAD_SILENCE`, `VOICE_VAD_PREROLL_MS` | `0.5`, `1.2`, `750` | Silero threshold, trailing silence in seconds, and retained lead-in in milliseconds. |
| `ORT_DYLIB_PATH` | platform library name | Path to a CPU ONNX Runtime shared library exposing the 1.24 API or later. |
| `PHONEMIZER_ESPEAK_LIBRARY` | platform library name | Optional path to eSpeak NG's shared library (Kokoro also needs the eSpeak NG voice data). |

Model layout matches upstream `earendil-works/pi`'s `apps/p2/voice-server/Dockerfile` (fork commit `780bf31fc`):

- `silero_vad.onnx`: sherpa-onnx's Silero V4 export.
- `parakeet/{encoder.int8.onnx,decoder.int8.onnx,joiner.int8.onnx,tokens.txt}`: sherpa-onnx's NeMo Parakeet TDT 0.6B v3 int8 export.
- `supertonic3/onnx/{duration_predictor.onnx,text_encoder.onnx,vector_estimator.onnx,vocoder.onnx,tts.json,unicode_indexer.json}` and `supertonic3/voice_styles/{F1..F5,M1..M5}.json`.
- `kokoro/{kokoro-v1.0.onnx,voices-v1.0.bin}`: Kokoro 82M v1.0.
- `pocket/{pocket_tts.onnx,pocket.json,<voice>.safetensors}`: Pocket TTS ONNX export and prompt-state `.safetensors` tensors (`F32`, `F16`, or `BF16`). The prompt states are the upstream layout: `<models>/pocket/<voice>.safetensors`, one file per voice of upstream's English catalog.

**Pocket:** `Config::load` loads `PocketOnnx` automatically whenever the ONNX weights are present alongside `<voice>.safetensors` prompt states, and skips `pocket` with a diagnostic only when the ONNX model file is not installed. The weights are `<models>/pocket/<onnx_model>` (default `pocket_tts.onnx`), else `model.onnx` or `pocket.onnx` in the same directory, with an existing file named by `VOICE_POCKET_MODEL` taking precedence. Optional `pocket.json` configures `sample_rate` (default `24000`), `chunk_samples` (default `1920`, 80 ms), `onnx_model`, `vocab`, `token_encoding` (`auto` | `codepoints` | `utf8_bytes` | `vocab`), `tokens_input`, `state_input`, `state_tensor`, `speed_input`, `steps_input`, `default_speed`, `audio_output`, `stream_tokens` and `state_output`. This crate does not shell out to Python.

**Pocket streaming:** `stream_tokens` makes the model run step by step: each step takes the next tokens and the state of the previous step, so the first audio arrives before the utterance is done, like upstream's Pocket TTS. The model's state output is named after its state input, or `state_output` names it; the audio arrives in `chunk_samples` pieces of 80 ms. Without `stream_tokens` the whole utterance is rendered in one pass.

## Pocket export and container build

The upstream image stores the checkpoint under `HF_HOME` and writes one prompt state per voice under `/models/pocket`. To reuse those states, copy the Pocket directory from an upstream image. With your upstream container ID in `UPSTREAM_CONTAINER`, run:

```sh
docker cp "${UPSTREAM_CONTAINER}:/models/pocket" ./pocket-export
```

Supply an ONNX graph exported from the same model revision. The upstream Dockerfile exports only the prompt states. A compatible graph accepts token IDs (int32 or int64, shape `[tokens]` or `[1,tokens]`), consumes the voice state as float tensors, and returns float PCM. Match the tokenizer to the model when preparing the export. In `pocket.json`, set `tokens_input`, `audio_output`, and the state bindings to the graph names. Keep external ONNX tensor data beside the graph at the relative paths referenced by the export. For a stateful streaming graph, set `stream_tokens` to its supported window size and map the returned state to the next call. A full-utterance graph should omit that setting.

Put the graph at `pocket-export/pocket_tts.onnx` alongside the prompts and configuration. From the workspace root:

```sh
docker buildx build --load -t pi-voice-server \
  --build-context pocket-export=./pocket-export \
  -f crates/pi-rs-voice-server/Dockerfile .
docker run --rm -p 127.0.0.1:8000:8000 -e VOICE_TOKEN pi-voice-server
```

Check enrollment with `curl -fsS -H "Authorization: Bearer $VOICE_TOKEN" http://localhost:8000/voices`. Exercise a listed Pocket voice with `POST /speak` before distributing the image. Building the image downloads the other engines and ONNX Runtime. The running service uses local files.

Build and run from the repository root:

```sh
cargo build --locked --release -p pi-rs-voice-server
VOICE_TOKEN=local-secret VOICE_MODELS=/models VOICE_ENGINES=supertonic,pocket,kokoro \
  ORT_DYLIB_PATH=/path/to/libonnxruntime.so ./target/release/pi-voice-server
```

## Verification

The offline transport and Pocket TTS tests exercise real loopback HTTP/WebSocket connections and `.safetensors` loading with deterministic inference. They do not require model weights, ONNX Runtime, or eSpeak NG. With Cargo dependencies cached:

```sh
cargo fmt --all -- --check
cargo clippy --offline --locked -p pi-rs-voice-server --all-targets -- -D warnings
cargo test --offline --locked -p pi-rs-voice-server
```

To also run the real ONNX Runtime end-to-end tests:

```sh
ORT_DYLIB_PATH=/path/to/libonnxruntime.so cargo test --offline --locked -p pi-rs-voice-server -- --include-ignored
```

Verified: formatting and Clippy are clean, 27 offline tests pass, and the 2 `#[ignore]` Pocket TTS ONNX Runtime tests pass with `libonnxruntime.so.1.31.1` (8/8 in `pocket_onnx -- --include-ignored`). Removing the previous-utterance clamp made the pre-roll regression fail; restoring it made the suite pass.

With the upstream weights and ONNX Runtime 1.28.2, the native executable returned non-silent PCM WAVs from Supertonic (44.1 kHz) and Kokoro (24 kHz). `/listen` detected and transcribed generated speech and returned the same transcript as sherpa-onnx 1.13.8 for the upstream Parakeet English reference clip.
