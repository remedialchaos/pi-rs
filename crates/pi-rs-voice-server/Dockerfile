# syntax=docker/dockerfile:1
# Build from the workspace root. Supply the Pocket export as a named build context.
FROM rust:bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN cargo build --release --locked -p pi-rs-voice-server

FROM python:3.12-slim-bookworm AS assets
RUN pip install --no-cache-dir --no-deps onnxruntime==1.31.0 \
    && mkdir /runtime && cp /usr/local/lib/python3.12/site-packages/onnxruntime/capi/libonnxruntime.so.* /runtime/libonnxruntime.so
ADD https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx /models/silero_vad.onnx
ADD https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2 /tmp/parakeet.tar.bz2
RUN mkdir -p /models/parakeet && python -c "import tarfile; tarfile.open('/tmp/parakeet.tar.bz2').extractall('/tmp', filter='data')" \
    && mv /tmp/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/*.onnx /tmp/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/tokens.txt /models/parakeet/
RUN python - <<'PY'
from pathlib import Path
from urllib.request import urlretrieve
files = ["onnx/" + name for name in ("duration_predictor.onnx", "text_encoder.onnx", "tts.json", "unicode_indexer.json", "vector_estimator.onnx", "vocoder.onnx")]
files += [f"voice_styles/{kind}{n}.json" for kind in "FM" for n in range(1, 6)]
for name in files:
    path = Path("/models/supertonic3") / name
    path.parent.mkdir(parents=True, exist_ok=True)
    urlretrieve("https://huggingface.co/Supertone/supertonic-3/resolve/main/" + name, path)
PY
ADD https://github.com/thewh1teagle/kokoro-onnx/releases/download/model-files-v1.0/kokoro-v1.0.onnx /models/kokoro/kokoro-v1.0.onnx
ADD https://github.com/thewh1teagle/kokoro-onnx/releases/download/model-files-v1.0/voices-v1.0.bin /models/kokoro/voices-v1.0.bin

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends libespeak-ng1 libgomp1 && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/pi-voice-server /usr/local/bin/
COPY --from=assets /runtime/ /usr/local/lib/
COPY --from=assets /models/ /models/
COPY --from=pocket-export / /models/pocket/
ENV VOICE_MODELS=/models ORT_DYLIB_PATH=/usr/local/lib/libonnxruntime.so
EXPOSE 8000
ENTRYPOINT ["pi-voice-server"]
