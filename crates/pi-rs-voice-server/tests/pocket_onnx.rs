#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use pi_rs_voice_server::{
    Error, Result, SttRecognizer, VadOutput, VoiceActivityDetector, VoiceServer, WINDOW,
    models::{
        Config, DEFAULT_POCKET_CHUNK_SAMPLES, DEFAULT_POCKET_ONNX_MODEL,
        DEFAULT_POCKET_SAMPLE_RATE, Pocket, PocketConfig, PocketOnnx, PocketSynthesis,
        PocketTokenEncoding, PocketVoiceState, PocketVoiceStateCache, VadConfig, init_runtime,
        load_safetensors_f32, parse_safetensors_f32,
    },
    router,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};
use tokio_util::sync::CancellationToken;

type Test<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const TOKEN: &str = "pocket-test-token";
const WAIT: Duration = Duration::from_secs(5);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Result<Self> {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pi-rs-voice-pocket-{label}-{}-{id}",
            std::process::id()
        ));
        fs::create_dir_all(&path)?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn encode_f32_to_f16_bits(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32;
    let frac = bits & 0x007f_ffff;
    if exp == 0xff {
        if frac == 0 {
            return sign | 0x7c00;
        }
        return sign | 0x7e00 | ((frac >> 13) as u16).max(1);
    }
    let half_exp = exp - 127 + 15;
    if half_exp >= 0x1f {
        return sign | 0x7c00;
    }
    if half_exp <= 0 {
        if half_exp < -10 {
            return sign;
        }
        let mant = (frac | 0x0080_0000) >> (1 - half_exp + 13);
        return sign | (mant as u16);
    }
    sign | ((half_exp as u16) << 10) | ((frac >> 13) as u16)
}

fn encode_f32_to_bf16_bits(value: f32) -> u16 {
    (value.to_bits() >> 16) as u16
}

fn build_safetensors(
    metadata: Option<Value>,
    tensors: &[(&str, &str, &[usize], &[u8])],
) -> Vec<u8> {
    let mut header = serde_json::Map::new();
    if let Some(meta) = metadata {
        header.insert("__metadata__".into(), meta);
    }
    let mut data = Vec::new();
    for &(name, dtype, shape, raw) in tensors {
        let start = data.len();
        data.extend_from_slice(raw);
        let end = data.len();
        header.insert(
            name.into(),
            json!({
                "dtype": dtype,
                "shape": shape,
                "data_offsets": [start, end],
            }),
        );
    }
    let header_bytes = serde_json::to_vec(&header).unwrap_or_default();
    let mut out = Vec::with_capacity(8 + header_bytes.len() + data.len());
    out.extend_from_slice(&(header_bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(&header_bytes);
    out.extend_from_slice(&data);
    out
}

fn f32_le_bytes(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn f16_le_bytes(values: &[f32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|&v| encode_f32_to_f16_bits(v).to_le_bytes())
        .collect()
}

fn bf16_le_bytes(values: &[f32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|&v| encode_f32_to_bf16_bits(v).to_le_bytes())
        .collect()
}

// --- Minimal pure-Rust ONNX protobuf builder (IR v8, opset 13) ---

const ONNX_FLOAT: u64 = 1;
const ONNX_INT64: u64 = 7;

enum OnnxDim<'a> {
    Value(i64),
    Param(&'a str),
}

fn pb_varint(buf: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            buf.push(byte);
            break;
        }
        buf.push(byte | 0x80);
    }
}

fn pb_tag(buf: &mut Vec<u8>, field: u32, wire_type: u8) {
    pb_varint(buf, (u64::from(field) << 3) | u64::from(wire_type));
}

fn pb_bytes(buf: &mut Vec<u8>, field: u32, data: &[u8]) {
    pb_tag(buf, field, 2);
    pb_varint(buf, data.len() as u64);
    buf.extend_from_slice(data);
}

fn pb_str(buf: &mut Vec<u8>, field: u32, s: &str) {
    pb_bytes(buf, field, s.as_bytes());
}

fn pb_u64(buf: &mut Vec<u8>, field: u32, v: u64) {
    pb_tag(buf, field, 0);
    pb_varint(buf, v);
}

fn onnx_value_info(name: &str, elem_type: u64, dims: &[OnnxDim<'_>]) -> Vec<u8> {
    let mut shape = Vec::new();
    for dim in dims {
        let mut d = Vec::new();
        match dim {
            OnnxDim::Value(v) => pb_u64(&mut d, 1, *v as u64),
            OnnxDim::Param(p) => pb_str(&mut d, 2, p),
        }
        pb_bytes(&mut shape, 1, &d);
    }
    let mut tensor_type = Vec::new();
    pb_u64(&mut tensor_type, 1, elem_type);
    pb_bytes(&mut tensor_type, 2, &shape);

    let mut ty = Vec::new();
    pb_bytes(&mut ty, 1, &tensor_type);

    let mut info = Vec::new();
    pb_str(&mut info, 1, name);
    pb_bytes(&mut info, 2, &ty);
    info
}

fn onnx_attr_int(name: &str, value: i64) -> Vec<u8> {
    let mut attr = Vec::new();
    pb_str(&mut attr, 1, name);
    pb_u64(&mut attr, 3, value as u64);
    pb_u64(&mut attr, 20, 2); // AttributeType::INT = 2
    attr
}

fn onnx_node(
    op_type: &str,
    name: &str,
    inputs: &[&str],
    outputs: &[&str],
    attrs: &[Vec<u8>],
) -> Vec<u8> {
    let mut node = Vec::new();
    for inp in inputs {
        pb_str(&mut node, 1, inp);
    }
    for out in outputs {
        pb_str(&mut node, 2, out);
    }
    pb_str(&mut node, 3, name);
    pb_str(&mut node, 4, op_type);
    for attr in attrs {
        pb_bytes(&mut node, 5, attr);
    }
    node
}

fn onnx_f32_initializer(name: &str, dims: &[i64], values: &[f32]) -> Vec<u8> {
    let mut tensor = Vec::new();
    for &dim in dims {
        pb_u64(&mut tensor, 1, dim as u64);
    }
    pb_u64(&mut tensor, 2, ONNX_FLOAT);
    pb_str(&mut tensor, 8, name);
    pb_bytes(&mut tensor, 9, &f32_le_bytes(values));
    tensor
}

fn onnx_model(
    graph_name: &str,
    nodes: &[Vec<u8>],
    initializers: &[Vec<u8>],
    inputs: &[Vec<u8>],
    outputs: &[Vec<u8>],
) -> Vec<u8> {
    let mut graph = Vec::new();
    for node in nodes {
        pb_bytes(&mut graph, 1, node);
    }
    pb_str(&mut graph, 2, graph_name);
    for init in initializers {
        pb_bytes(&mut graph, 5, init);
    }
    for inp in inputs {
        pb_bytes(&mut graph, 11, inp);
    }
    for out in outputs {
        pb_bytes(&mut graph, 12, out);
    }

    let mut opset = Vec::new();
    pb_u64(&mut opset, 2, 13); // opset 13

    let mut model = Vec::new();
    pb_u64(&mut model, 1, 8); // IR version 8
    pb_bytes(&mut model, 7, &graph);
    pb_bytes(&mut model, 8, &opset);
    model
}

/// Builds a minimal ONNX graph:
/// `audio = prompt_state + mean(Cast(tokens, float32)) * 0.001`
fn build_pocket_onnx_model() -> Vec<u8> {
    let nodes = [
        onnx_node(
            "Cast",
            "cast_tokens",
            &["tokens"],
            &["tokens_f32"],
            &[onnx_attr_int("to", ONNX_FLOAT as i64)],
        ),
        onnx_node(
            "ReduceMean",
            "mean_tokens",
            &["tokens_f32"],
            &["mean_tok"],
            &[],
        ),
        onnx_node(
            "Mul",
            "scale_mean",
            &["mean_tok", "scale"],
            &["tok_bias"],
            &[],
        ),
        onnx_node(
            "Add",
            "add_bias",
            &["prompt_state", "tok_bias"],
            &["audio"],
            &[],
        ),
    ];
    let initializers = [onnx_f32_initializer("scale", &[1], &[0.001])];
    let inputs = [
        onnx_value_info(
            "tokens",
            ONNX_INT64,
            &[OnnxDim::Value(1), OnnxDim::Param("seq")],
        ),
        onnx_value_info(
            "prompt_state",
            ONNX_FLOAT,
            &[OnnxDim::Value(1), OnnxDim::Param("samples")],
        ),
    ];
    let outputs = [onnx_value_info(
        "audio",
        ONNX_FLOAT,
        &[OnnxDim::Value(1), OnnxDim::Param("samples")],
    )];
    onnx_model("pocket_test", &nodes, &initializers, &inputs, &outputs)
}

/// Builds a minimal ONNX graph with `speed` and `steps` inputs:
/// `audio = prompt_state * speed + mean(Cast(tokens, float32)) * 0.001 + Cast(steps, float32) * 0.01`
fn build_pocket_onnx_with_speed_and_steps() -> Vec<u8> {
    let nodes = [
        onnx_node(
            "Cast",
            "cast_tokens",
            &["tokens"],
            &["tokens_f32"],
            &[onnx_attr_int("to", ONNX_FLOAT as i64)],
        ),
        onnx_node(
            "ReduceMean",
            "mean_tokens",
            &["tokens_f32"],
            &["mean_tok"],
            &[],
        ),
        onnx_node(
            "Mul",
            "scale_mean",
            &["mean_tok", "scale"],
            &["tok_bias"],
            &[],
        ),
        onnx_node(
            "Mul",
            "scale_state",
            &["prompt_state", "speed"],
            &["scaled_state"],
            &[],
        ),
        onnx_node(
            "Cast",
            "cast_steps",
            &["steps"],
            &["steps_f32"],
            &[onnx_attr_int("to", ONNX_FLOAT as i64)],
        ),
        onnx_node(
            "Mul",
            "scale_steps",
            &["steps_f32", "step_scale"],
            &["step_bias"],
            &[],
        ),
        onnx_node(
            "Add",
            "add_tok_bias",
            &["scaled_state", "tok_bias"],
            &["with_tok"],
            &[],
        ),
        onnx_node(
            "Add",
            "add_step_bias",
            &["with_tok", "step_bias"],
            &["audio"],
            &[],
        ),
    ];
    let initializers = [
        onnx_f32_initializer("scale", &[1], &[0.001]),
        onnx_f32_initializer("step_scale", &[1], &[0.01]),
    ];
    let inputs = [
        onnx_value_info(
            "tokens",
            ONNX_INT64,
            &[OnnxDim::Value(1), OnnxDim::Param("seq")],
        ),
        onnx_value_info(
            "prompt_state",
            ONNX_FLOAT,
            &[OnnxDim::Value(1), OnnxDim::Param("samples")],
        ),
        onnx_value_info("speed", ONNX_FLOAT, &[OnnxDim::Value(1)]),
        onnx_value_info("steps", ONNX_INT64, &[OnnxDim::Value(1)]),
    ];
    let outputs = [onnx_value_info(
        "audio",
        ONNX_FLOAT,
        &[OnnxDim::Value(1), OnnxDim::Param("samples")],
    )];
    onnx_model(
        "pocket_speed_steps",
        &nodes,
        &initializers,
        &inputs,
        &outputs,
    )
}

struct DummyVad;
impl VoiceActivityDetector for DummyVad {
    fn accept(&mut self, _: &[f32; WINDOW]) -> Result<VadOutput> {
        Ok(VadOutput {
            speaking: false,
            segment: None,
        })
    }
    fn reset(&mut self) {}
}

struct DummyStt;
impl SttRecognizer for DummyStt {
    fn transcribe(&self, _: &[f32]) -> Result<String> {
        Ok(String::new())
    }
}

struct RunningServer {
    http: String,
    ws: String,
    task: JoinHandle<std::io::Result<()>>,
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start_server(engine: pi_rs_voice_server::Engine) -> Test<RunningServer> {
    let server = VoiceServer::new(
        TOKEN,
        vec![engine],
        "pocket",
        None,
        || Ok(Box::new(DummyVad)),
        0.0,
        Arc::new(DummyStt),
    )?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let task = tokio::spawn(async { axum::serve(listener, router(server)).await });
    Ok(RunningServer {
        http: format!("http://{addr}"),
        ws: format!("ws://{addr}"),
        task,
    })
}

fn http_client() -> Test<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .no_proxy()
        .timeout(WAIT)
        .build()?)
}

// --- Offline tests (always run, no ONNX Runtime dylib needed) ---

#[test]
fn safetensors_parses_f32_f16_bf16_and_skips_metadata() -> Result<()> {
    let f32_vals = [0.25f32, -0.5, 0.75, 1.0];
    let f16_vals = [0.5f32, -1.25];
    let bf16_vals = [2.0f32, -3.5];

    let bytes = build_safetensors(
        Some(json!({"format": "pt", "voice": "alba"})),
        &[
            ("prompt_state", "F32", &[1, 4], &f32_le_bytes(&f32_vals)),
            ("half_state", "F16", &[1, 2], &f16_le_bytes(&f16_vals)),
            ("bf_state", "BF16", &[2], &bf16_le_bytes(&bf16_vals)),
        ],
    );

    let parsed = parse_safetensors_f32(&bytes)?;
    assert_eq!(parsed.len(), 3);
    assert!(!parsed.contains_key("__metadata__"));
    assert_eq!(parsed["prompt_state"].shape, vec![1, 4]);
    assert_eq!(parsed["prompt_state"].data, f32_vals);
    assert_eq!(parsed["half_state"].shape, vec![1, 2]);
    assert_eq!(parsed["half_state"].data, f16_vals);
    assert_eq!(parsed["bf_state"].shape, vec![2]);
    assert_eq!(parsed["bf_state"].data, bf16_vals);

    let state = PocketVoiceState::from_bytes(&bytes)?;
    let (name, primary) = state.primary_tensor(None)?;
    assert_eq!(name, "prompt_state");
    assert_eq!(primary.data, f32_vals);
    assert_eq!(state.primary_tensor(Some("half_state"))?.0, "half_state");
    assert!(matches!(
        state.primary_tensor(Some("missing")),
        Err(Error::Model(_))
    ));
    Ok(())
}

#[test]
fn safetensors_rejects_malformed_truncated_oob_and_unsupported_dtypes() {
    assert!(matches!(
        parse_safetensors_f32(&[1, 2, 3]),
        Err(Error::Model(_))
    ));
    assert!(matches!(
        parse_safetensors_f32(&0u64.to_le_bytes()),
        Err(Error::Model(_))
    ));
    let mut truncated_header = Vec::new();
    truncated_header.extend_from_slice(&64u64.to_le_bytes());
    truncated_header.extend_from_slice(b"{\"x\":");
    assert!(matches!(
        parse_safetensors_f32(&truncated_header),
        Err(Error::Model(_))
    ));

    let bad_json = b"{not-json";
    let mut bad_json_buf = Vec::new();
    bad_json_buf.extend_from_slice(&(bad_json.len() as u64).to_le_bytes());
    bad_json_buf.extend_from_slice(bad_json);
    assert!(matches!(
        parse_safetensors_f32(&bad_json_buf),
        Err(Error::Model(_))
    ));

    let empty_obj = b"{}";
    let mut empty_buf = Vec::new();
    empty_buf.extend_from_slice(&(empty_obj.len() as u64).to_le_bytes());
    empty_buf.extend_from_slice(empty_obj);
    assert!(matches!(
        parse_safetensors_f32(&empty_buf),
        Err(Error::Model(_))
    ));

    // Out-of-bounds data_offsets
    let oob_header = serde_json::to_vec(&json!({
        "prompt_state": {
            "dtype": "F32",
            "shape": [1, 2],
            "data_offsets": [0, 8],
        }
    }))
    .unwrap_or_default();
    let mut oob_buf = Vec::new();
    oob_buf.extend_from_slice(&(oob_header.len() as u64).to_le_bytes());
    oob_buf.extend_from_slice(&oob_header);
    oob_buf.extend_from_slice(&[0u8; 4]); // too short!
    assert!(matches!(
        parse_safetensors_f32(&oob_buf),
        Err(Error::Model(_))
    ));

    // Unsupported dtype
    let int_buf = build_safetensors(None, &[("prompt_state", "I64", &[1], &[0u8; 8])]);
    assert!(matches!(
        parse_safetensors_f32(&int_buf),
        Err(Error::Model(_))
    ));
}

#[test]
fn voice_state_cache_reuses_parsed_tensors_and_rejects_unknown_voice() -> Result<()> {
    let dir = TempDir::new("cache")?;
    let alba_path = dir.path().join("alba.safetensors");
    let payload = build_safetensors(
        None,
        &[(
            "prompt_state",
            "F32",
            &[1, 3],
            &f32_le_bytes(&[0.1, 0.2, 0.3]),
        )],
    );
    fs::write(&alba_path, payload)?;

    let loaded = load_safetensors_f32(&alba_path)?;
    assert_eq!(loaded["prompt_state"].data, vec![0.1, 0.2, 0.3]);

    let mut cache = PocketVoiceStateCache::new();
    assert!(cache.is_empty());
    let first = cache.load("alba", &alba_path)?;
    assert_eq!(cache.len(), 1);
    assert!(cache.contains("alba"));

    // Delete file to prove second load hits the cache.
    fs::remove_file(&alba_path)?;
    let second = cache.load("alba", &alba_path)?;
    assert!(Arc::ptr_eq(&first, &second));

    struct NoopSynth;
    impl PocketSynthesis for NoopSynth {
        fn generate(
            &mut self,
            _: &Path,
            _: &str,
            _: &CancellationToken,
            _: &mut dyn FnMut(&[f32]) -> bool,
        ) -> Result<()> {
            Ok(())
        }
    }
    fs::write(dir.path().join("alba.safetensors"), [])?;
    let engine = Pocket::engine(NoopSynth, dir.path().to_path_buf(), 24_000)?;
    assert_eq!(engine.info.id, "pocket");
    assert_eq!(engine.info.default_voice, "alba");
    Ok(())
}

#[test]
fn pocket_config_loads_defaults_overrides_and_rejects_invalid_values() -> Result<()> {
    let dir = TempDir::new("config")?;
    let defaults = PocketConfig::load(dir.path())?;
    assert_eq!(defaults.sample_rate, DEFAULT_POCKET_SAMPLE_RATE);
    assert_eq!(defaults.chunk_samples, DEFAULT_POCKET_CHUNK_SAMPLES);
    assert_eq!(defaults.onnx_model, DEFAULT_POCKET_ONNX_MODEL);
    assert_eq!(defaults.token_encoding, PocketTokenEncoding::Auto);

    fs::write(
        dir.path().join("pocket.json"),
        serde_json::to_vec(&json!({
            "sample_rate": 16000,
            "chunk_samples": 1024,
            "onnx_model": "custom.onnx",
            "token_encoding": "utf8_bytes",
            "default_speed": 1.25
        }))?,
    )?;
    let custom = PocketConfig::load(dir.path())?;
    assert_eq!(custom.sample_rate, 16_000);
    assert_eq!(custom.chunk_samples, 1024);
    assert_eq!(
        custom.model_path(dir.path()),
        dir.path().join("custom.onnx")
    );
    assert_eq!(custom.token_encoding, PocketTokenEncoding::Utf8Bytes);
    assert_eq!(custom.default_speed, 1.25);

    for bad in [
        json!({"sample_rate": 0}),
        json!({"chunk_samples": 0}),
        json!({"onnx_model": "   "}),
        json!({"default_speed": 0.0}),
        json!({"token_encoding": "vocab"}),
    ] {
        fs::write(dir.path().join("pocket.json"), serde_json::to_vec(&bad)?)?;
        assert!(matches!(
            PocketConfig::load(dir.path()),
            Err(Error::Config(_))
        ));
    }
    Ok(())
}

#[test]
fn config_load_engines_skips_pocket_when_onnx_missing_and_fails_on_bad_config() -> Result<()> {
    let dir = TempDir::new("load-engines")?;
    let pocket_dir = dir.path().join("pocket");
    fs::create_dir_all(&pocket_dir)?;
    fs::write(
        pocket_dir.join("alba.safetensors"),
        build_safetensors(
            None,
            &[("prompt_state", "F32", &[1, 2], &f32_le_bytes(&[0.1, 0.2]))],
        ),
    )?;

    let cfg = Config {
        models: dir.path().to_path_buf(),
        engines: vec!["pocket".into()],
        default_engine: "pocket".into(),
        default_voice: None,
        tts_threads: 1,
        tts_steps: 1,
        vad: VadConfig::default(),
        preroll_ms: 0.0,
        port: 0,
    };
    let err = cfg
        .load_engines()
        .err()
        .ok_or_else(|| Error::Config("expected Config error".into()))?;
    assert!(matches!(err, Error::Config(_)));

    // Invalid pocket.json fails with Config error rather than silently skipping.
    fs::write(pocket_dir.join("pocket.json"), "{invalid")?;
    let err = cfg
        .load_engines()
        .err()
        .ok_or_else(|| Error::Config("expected Config error".into()))?;
    assert!(matches!(err, Error::Config(_)));
    Ok(())
}

#[tokio::test]
async fn pocket_engine_streams_safetensors_voice_over_http_and_websocket() -> Test {
    struct SafetensorsMock {
        chunk_samples: usize,
    }
    impl PocketSynthesis for SafetensorsMock {
        fn generate(
            &mut self,
            state: &Path,
            _text: &str,
            stop: &CancellationToken,
            emit: &mut dyn FnMut(&[f32]) -> bool,
        ) -> Result<()> {
            let parsed = PocketVoiceState::load(state)?;
            let (_, tensor) = parsed.primary_tensor(None)?;
            for chunk in tensor.data.chunks(self.chunk_samples) {
                if stop.is_cancelled() || !emit(chunk) {
                    break;
                }
            }
            Ok(())
        }
    }

    let dir = TempDir::new("mock-http")?;
    let samples = [0.25f32, -0.5, 0.5, -0.25, 0.75];
    fs::write(
        dir.path().join("alba.safetensors"),
        build_safetensors(
            None,
            &[("prompt_state", "F32", &[1, 5], &f32_le_bytes(&samples))],
        ),
    )?;
    let engine = Pocket::engine(
        SafetensorsMock { chunk_samples: 2 },
        dir.path().to_path_buf(),
        24_000,
    )?;
    let server = start_server(engine).await?;
    let client = http_client()?;

    let resp = client
        .post(format!("{}/speak", server.http))
        .bearer_auth(TOKEN)
        .json(&json!({"text": "hello pocket"}))
        .send()
        .await?;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-voice"], "pocket-alba");
    let wav = resp.bytes().await?;
    assert_eq!(&wav[..4], b"RIFF");
    assert_eq!(wav.len(), 44 + samples.len() * 2);

    let mut req = format!("{}/speak/stream", server.ws).into_client_request()?;
    req.headers_mut()
        .insert("authorization", format!("Bearer {TOKEN}").parse()?);
    let (mut ws, _) = timeout(WAIT, connect_async(req)).await??;
    let ready: Value =
        serde_json::from_str(timeout(WAIT, ws.next()).await?.ok_or("eof")??.to_text()?)?;
    assert_eq!(ready["type"], "ready");

    use futures_util::SinkExt;
    ws.send(Message::Text(
        json!({"type": "speak", "id": "job1", "text": "hi"})
            .to_string()
            .into(),
    ))
    .await?;
    let start: Value =
        serde_json::from_str(timeout(WAIT, ws.next()).await?.ok_or("eof")??.to_text()?)?;
    assert_eq!(start["type"], "start");
    assert_eq!(start["voice"], "pocket-alba");

    let mut frames = Vec::new();
    loop {
        let msg = timeout(WAIT, ws.next()).await?.ok_or("eof")??;
        match msg {
            Message::Binary(bin) => frames.push(bin),
            Message::Text(txt) => {
                let done: Value = serde_json::from_str(txt.as_str())?;
                assert_eq!(done["type"], "done");
                assert_eq!(done["cancelled"], false);
                break;
            }
            other => return Err(format!("unexpected ws message: {other:?}").into()),
        }
    }
    assert_eq!(frames.len(), 3);
    Ok(())
}

// --- Real `ort` ONNX Runtime end-to-end tests ---

fn expected_pcm16_wav_payload(samples: &[f32]) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|&s| ((s.clamp(-1.0, 1.0) * 32_767.0) as i16).to_le_bytes())
        .collect()
}

#[tokio::test]
#[ignore = "requires ONNX Runtime shared library (set ORT_DYLIB_PATH and run with -- --include-ignored)"]
async fn pocket_onnx_end_to_end_http_and_websocket_synthesis() -> Test {
    init_runtime(None)?;

    let dir = TempDir::new("ort-e2e")?;
    let pocket_dir = dir.path().join("pocket");
    fs::create_dir_all(&pocket_dir)?;

    fs::write(
        pocket_dir.join("pocket_tts.onnx"),
        build_pocket_onnx_model(),
    )?;
    fs::write(
        pocket_dir.join("vocab.json"),
        serde_json::to_vec(&json!({
            "h": 10,
            "i": 20,
            "!": 30,
        }))?,
    )?;
    fs::write(
        pocket_dir.join("pocket.json"),
        serde_json::to_vec(&json!({
            "sample_rate": 24000,
            "chunk_samples": 4,
            "onnx_model": "pocket_tts.onnx",
            "vocab": "vocab.json",
            "token_encoding": "vocab",
        }))?,
    )?;

    let alba_state = [
        -0.5f32, -0.25, 0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.8, 1.5, // 1.5 tests clamping to 1.0
    ];
    let vera_state = [
        -0.25f32, 0.25, -0.125, 0.125, 0.0, 0.5, -0.5, 0.25, 0.0, 0.5,
    ];
    fs::write(
        pocket_dir.join("alba.safetensors"),
        build_safetensors(
            None,
            &[("prompt_state", "F32", &[1, 10], &f32_le_bytes(&alba_state))],
        ),
    )?;
    fs::write(
        pocket_dir.join("vera.safetensors"),
        build_safetensors(
            None,
            &[("prompt_state", "F16", &[1, 10], &f16_le_bytes(&vera_state))],
        ),
    )?;

    let cfg = Config {
        models: dir.path().to_path_buf(),
        engines: vec!["pocket".into()],
        default_engine: "pocket".into(),
        default_voice: None,
        tts_threads: 1,
        tts_steps: 5,
        vad: VadConfig::default(),
        preroll_ms: 0.0,
        port: 0,
    };
    let mut engines = cfg.load_engines()?;
    assert_eq!(engines.len(), 1);
    let pocket_engine = engines.pop().ok_or("missing loaded pocket engine")?;
    let server = start_server(pocket_engine).await?;
    let client = http_client()?;

    // Verify /voices catalog
    let catalog: Value = client
        .get(format!("{}/voices", server.http))
        .bearer_auth(TOKEN)
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(
        catalog["default"],
        json!({"engine": "pocket", "voice": "pocket-alba"})
    );
    assert_eq!(catalog["engines"][0]["streaming"], true);
    assert_eq!(catalog["engines"][0]["sampleRate"], 24000);

    // "hi!" -> tokens [10, 20, 30] -> mean = 20.0 -> bias = 0.02
    let bias = 20.0f32 * 0.001;
    let alba_expected: Vec<f32> = alba_state.iter().map(|&s| s + bias).collect();
    let vera_expected: Vec<f32> = vera_state.iter().map(|&s| s + bias).collect();

    // POST /speak for alba (F32)
    let resp = client
        .post(format!("{}/speak", server.http))
        .bearer_auth(TOKEN)
        .json(&json!({"text": "hi!", "voice": "pocket-alba"}))
        .send()
        .await?;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-voice"], "pocket-alba");
    let wav = resp.bytes().await?;
    assert_eq!(&wav[..4], b"RIFF");
    assert_eq!(&wav[24..28], &24_000u32.to_le_bytes());
    assert_eq!(
        &wav[44..],
        expected_pcm16_wav_payload(&alba_expected).as_slice()
    );

    // POST /speak for vera (F16)
    let resp = client
        .post(format!("{}/speak", server.http))
        .bearer_auth(TOKEN)
        .json(&json!({"text": "hi!", "voice": "pocket-vera"}))
        .send()
        .await?;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-voice"], "pocket-vera");
    let wav = resp.bytes().await?;
    assert_eq!(
        &wav[44..],
        expected_pcm16_wav_payload(&vera_expected).as_slice()
    );

    // WS /speak/stream for chunked streaming (10 samples / chunk_samples=4 -> 4, 4, 2)
    let mut req = format!("{}/speak/stream", server.ws).into_client_request()?;
    req.headers_mut()
        .insert("authorization", format!("Bearer {TOKEN}").parse()?);
    let (mut ws, _) = timeout(WAIT, connect_async(req)).await??;
    let _ready = timeout(WAIT, ws.next()).await?.ok_or("eof")??;

    use futures_util::SinkExt;
    ws.send(Message::Text(
        json!({"type": "speak", "id": "ort1", "text": "hi!", "voice": "pocket-alba"})
            .to_string()
            .into(),
    ))
    .await?;
    let start: Value =
        serde_json::from_str(timeout(WAIT, ws.next()).await?.ok_or("eof")??.to_text()?)?;
    assert_eq!(start["type"], "start");
    assert_eq!(start["engine"], "pocket");

    let mut chunk_sizes = Vec::new();
    let mut pcm_bytes = Vec::new();
    loop {
        match timeout(WAIT, ws.next()).await?.ok_or("eof")?? {
            Message::Binary(frame) => {
                assert_eq!(frame[0] as usize, 4);
                assert_eq!(&frame[1..5], b"ort1");
                let payload = &frame[5..];
                chunk_sizes.push(payload.len() / 2);
                pcm_bytes.extend_from_slice(payload);
            }
            Message::Text(txt) => {
                let done: Value = serde_json::from_str(txt.as_str())?;
                assert_eq!(done["type"], "done");
                assert_eq!(done["cancelled"], false);
                break;
            }
            other => return Err(format!("unexpected ws frame: {other:?}").into()),
        }
    }
    assert_eq!(chunk_sizes, vec![4, 4, 2]);
    assert_eq!(pcm_bytes, expected_pcm16_wav_payload(&alba_expected));
    Ok(())
}

#[test]
#[ignore = "requires ONNX Runtime shared library (set ORT_DYLIB_PATH and run with -- --include-ignored)"]
fn pocket_onnx_optional_speed_steps_cancellation_caching_and_non_finite_guard() -> Test {
    init_runtime(None)?;

    let dir = TempDir::new("ort-opts")?;
    let pocket_dir = dir.path();
    fs::write(
        pocket_dir.join("pocket_tts.onnx"),
        build_pocket_onnx_with_speed_and_steps(),
    )?;
    fs::write(
        pocket_dir.join("pocket.json"),
        serde_json::to_vec(&json!({
            "sample_rate": 24000,
            "chunk_samples": 2,
            "default_speed": 1.25,
            "token_encoding": "utf8_bytes",
        }))?,
    )?;

    let alba_state = [0.2f32, -0.2, 0.4, -0.4, 0.1];
    let alba_path = pocket_dir.join("alba.safetensors");
    fs::write(
        &alba_path,
        build_safetensors(
            None,
            &[("prompt_state", "F32", &[1, 5], &f32_le_bytes(&alba_state))],
        ),
    )?;
    let nan_path = pocket_dir.join("vera.safetensors");
    fs::write(
        &nan_path,
        build_safetensors(
            None,
            &[(
                "prompt_state",
                "F32",
                &[1, 5],
                &f32_le_bytes(&[0.1, f32::NAN, 0.2, 0.3, 0.4]),
            )],
        ),
    )?;

    let mut adapter = PocketOnnx::load(pocket_dir, 1, 7)?;
    assert_eq!(adapter.sample_rate(), 24_000);
    assert_eq!(adapter.cached_voices(), 0);

    // "AB" -> UTF-8 bytes [65, 66] -> mean = 65.5 -> tok_bias = 0.0655; steps=7 -> step_bias=0.07
    let stop = CancellationToken::new();
    let mut collected = Vec::new();
    adapter.generate(&alba_path, "AB", &stop, &mut |chunk| {
        collected.extend_from_slice(chunk);
        true
    })?;
    assert_eq!(adapter.cached_voices(), 1);
    let expected: Vec<f32> = alba_state
        .iter()
        .map(|&s| (s * 1.25 + 0.0655 + 0.07).clamp(-1.0, 1.0))
        .collect();
    for (got, want) in collected.iter().zip(expected.iter()) {
        assert!((got - want).abs() < 1e-5, "got {got}, expected {want}");
    }

    // Remove file to verify cached voice state is reused, and test early sink stop.
    fs::remove_file(&alba_path)?;
    let mut partial = Vec::new();
    adapter.generate(&alba_path, "AB", &stop, &mut |chunk| {
        partial.push(chunk.to_vec());
        false
    })?;
    assert_eq!(partial.len(), 1);
    assert_eq!(partial[0].len(), 2);

    // CancellationToken stops after first chunk.
    let cancel = CancellationToken::new();
    let mut cancelled_chunks = Vec::new();
    adapter.generate(&alba_path, "AB", &cancel, &mut |chunk| {
        cancelled_chunks.push(chunk.to_vec());
        cancel.cancel();
        true
    })?;
    assert_eq!(cancelled_chunks.len(), 1);

    // Non-finite output fails cleanly before emitting any chunk.
    let mut nan_chunks = Vec::new();
    let err = adapter
        .generate(&nan_path, "AB", &stop, &mut |chunk| {
            nan_chunks.push(chunk.to_vec());
            true
        })
        .unwrap_err();
    assert!(matches!(err, Error::Model(_)));
    assert!(nan_chunks.is_empty());
    Ok(())
}
