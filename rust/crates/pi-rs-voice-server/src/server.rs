use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::stream::{FuturesOrdered, SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use rayon::{ThreadPool, ThreadPoolBuilder};
use serde::Serialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time;
use tokio_util::sync::CancellationToken;

use crate::audio::{append_pcm16, riff_header};
use crate::engine::valid_id;
use crate::text::python_space;
use crate::{
    Engine, Error, Result, SAMPLE_RATE, Segmenter, SegmenterEvent, SttRecognizer, Voice,
    VoiceActivityDetector, pcm16_to_float,
};

const BODY_LIMIT: usize = 64 * 1024;
const LISTEN_LIMIT: usize = 1 << 20;
const MAX_TEXT: usize = 2000;
const HEARTBEAT: Duration = Duration::from_secs(20);

pub type VadFactory = dyn Fn() -> Result<Box<dyn VoiceActivityDetector>> + Send + Sync;

#[derive(Clone)]
pub struct VoiceServer(Arc<Inner>);

struct Inner {
    authorization: Vec<u8>,
    engines: Vec<Arc<Engine>>,
    default_engine: usize,
    default_voice: String,
    vad: Box<VadFactory>,
    preroll_ms: f64,
    recognizer: Arc<dyn SttRecognizer>,
    tts: ThreadPool,
    stt: ThreadPool,
}

impl VoiceServer {
    pub fn new(
        token: &str,
        engines: Vec<Engine>,
        default_engine: &str,
        default_voice: Option<&str>,
        vad: impl Fn() -> Result<Box<dyn VoiceActivityDetector>> + Send + Sync + 'static,
        preroll_ms: f64,
        recognizer: Arc<dyn SttRecognizer>,
    ) -> Result<Self> {
        if engines.is_empty() {
            return Err(Error::Config("no voice engines were loaded".into()));
        }
        let default_engine = engines
            .iter()
            .position(|engine| engine.info.id == default_engine)
            .unwrap_or(0);
        let default_info = &engines[default_engine].info;
        let default_voice = default_voice
            .filter(|&voice| default_info.voices.iter().any(|v| v.id == voice))
            .unwrap_or(&default_info.default_voice)
            .to_owned();
        let pool = |threads, name| {
            ThreadPoolBuilder::new()
                .num_threads(threads)
                .thread_name(move |n| format!("voice-{name}-{n}"))
                .build()
        };
        Ok(Self(Arc::new(Inner {
            authorization: format!("Bearer {token}").into_bytes(),
            engines: engines.into_iter().map(Arc::new).collect(),
            default_engine,
            default_voice,
            vad: Box::new(vad),
            preroll_ms,
            recognizer,
            tts: pool(4, "tts")?,
            stt: pool(2, "stt")?,
        })))
    }

    fn request(&self, body: &Value) -> Result<SpeakRequest> {
        let text = body
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| bad("expected {text, voice?, engine?, speed?}"))?;
        let text = text.trim_matches(python_space);
        if text.is_empty() || text.chars().take(MAX_TEXT + 1).count() > MAX_TEXT {
            return Err(bad("invalid text"));
        }
        let engine = body.get("engine").filter(|v| truthy(v)).map(python_string);
        let voice = body.get("voice").filter(|v| truthy(v)).map(python_string);
        let find = |id| {
            self.0
                .engines
                .iter()
                .position(|engine| engine.info.id == id)
        };
        let (index, voice) = if let Some(voice) = voice {
            let Some(((prefix, name), index)) = voice
                .split_once('-')
                .and_then(|(prefix, name)| find(prefix).map(|index| ((prefix, name), index)))
                .filter(|((_, name), index)| {
                    valid_id(&voice, 32)
                        && self.0.engines[*index]
                            .info
                            .voices
                            .iter()
                            .any(|v| v.id == *name)
                })
            else {
                return Err(bad(format!("unknown voice {voice}")));
            };
            if engine.as_deref().is_some_and(|id| id != prefix) {
                return Err(bad("voice does not belong to engine"));
            }
            (index, name.to_owned())
        } else {
            let index = match engine.as_deref() {
                Some(id) => find(id).ok_or_else(|| bad(format!("unknown engine {id}")))?,
                None => self.0.default_engine,
            };
            let name = if index == self.0.default_engine {
                &self.0.default_voice
            } else {
                &self.0.engines[index].info.default_voice
            };
            (index, name.clone())
        };
        let speed = body
            .get("speed")
            .filter(|v| !v.is_null())
            .map(|speed| {
                let value = match speed {
                    Value::String(s) => python_float(s),
                    Value::Bool(b) => Some(if *b { 1. } else { 0. }),
                    _ => speed.as_f64(),
                };
                value
                    .filter(|v| (0.5..=2.0).contains(v))
                    .map(|v| v as f32)
                    .ok_or_else(|| bad("invalid speed"))
            })
            .transpose()?;
        Ok(SpeakRequest {
            engine: self.0.engines[index].clone(),
            voice,
            text: text.into(),
            speed,
        })
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::String(value) => !value.is_empty(),
        Value::Number(value) => value.as_f64() != Some(0.),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn python_string(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        _ => value.to_string(),
    }
}

fn python_float(text: &str) -> Option<f64> {
    let text = text.trim_matches(python_space);
    if !text.contains('_') {
        return text.parse().ok();
    }
    let bytes = text.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'_'
            && (i == 0
                || !bytes[i - 1].is_ascii_digit()
                || !bytes.get(i + 1).is_some_and(u8::is_ascii_digit))
        {
            return None;
        }
    }
    text.replace('_', "").parse().ok()
}

fn bad(error: impl Into<String>) -> Error {
    Error::Request(error.into())
}

pub fn router(server: VoiceServer) -> Router {
    Router::new()
        .route("/health", get(|| async { Json(json!({"ok":true})) }))
        .route("/voices", get(voices))
        .route("/speak", post(speak))
        .route("/speak/stream", get(speak_upgrade))
        .route("/listen", get(listen_upgrade))
        .fallback(|| async { (StatusCode::NOT_FOUND, "404: Not Found") })
        .layer(middleware::from_fn_with_state(server.clone(), auth))
        .with_state(server)
}

async fn auth(State(server): State<VoiceServer>, request: Request, next: Next) -> Response {
    let public = request.method() == Method::GET && request.uri().path() == "/health";
    let header = request
        .headers()
        .get(header::AUTHORIZATION)
        .map_or(&[][..], HeaderValue::as_bytes);
    if public || bool::from(header.ct_eq(&server.0.authorization)) {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"unauthorized"})),
        )
            .into_response()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CatalogEngine<'a> {
    id: &'a str,
    name: &'a str,
    streaming: bool,
    sample_rate: u32,
    voices: Vec<Voice>,
}

async fn voices(State(server): State<VoiceServer>) -> Json<Value> {
    let state = &server.0;
    let engines: Vec<_> = state
        .engines
        .iter()
        .enumerate()
        .map(|(index, engine)| {
            let info = &engine.info;
            let default = if index == state.default_engine {
                &state.default_voice
            } else {
                &info.default_voice
            };
            let order = info
                .voices
                .iter()
                .filter(|v| &v.id == default)
                .chain(info.voices.iter().filter(|v| &v.id != default));
            CatalogEngine {
                id: &info.id,
                name: &info.name,
                streaming: info.streaming,
                sample_rate: info.sample_rate,
                voices: order
                    .map(|v| Voice {
                        id: format!("{}-{}", info.id, v.id),
                        name: v.name.clone(),
                    })
                    .collect(),
            }
        })
        .collect();
    let id = &state.engines[state.default_engine].info.id;
    Json(
        json!({"engines":engines,"default":{"engine":id,"voice":format!("{id}-{}", state.default_voice)}}),
    )
}

struct SpeakRequest {
    engine: Arc<Engine>,
    voice: String,
    text: String,
    speed: Option<f32>,
}

impl SpeakRequest {
    fn voice_id(&self) -> String {
        format!("{}-{}", self.engine.info.id, self.voice)
    }
    fn stream(
        &self,
        stop: &CancellationToken,
        chunk: &mut dyn FnMut(&[f32]) -> bool,
    ) -> Result<()> {
        self.engine
            .stream(&self.text, &self.voice, self.speed, stop, chunk)
    }
}

async fn speak(State(server): State<VoiceServer>, body: Body) -> Response {
    let Ok(body) = to_bytes(body, BODY_LIMIT).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let request = serde_json::from_slice(&body)
        .map_err(Error::from)
        .and_then(|body| server.request(&body));
    let request = match request {
        Ok(request) => request,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":error.to_string()})),
            )
                .into_response();
        }
    };
    let start = Instant::now();
    let voice = request.voice_id();
    let rate = request.engine.info.sample_rate;
    let rx = dispatch(&server.0.tts, move || {
        let mut wav = vec![0; 44];
        request.stream(&CancellationToken::new(), &mut |samples| {
            append_pcm16(&mut wav, samples);
            true
        })?;
        let header = riff_header(rate, wav.len() - 44)?;
        wav[..44].copy_from_slice(&header);
        Ok::<_, Error>(wav)
    });
    let bytes = match rx.await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(error)) => {
            eprintln!("voice synthesis failed ({voice}): {error}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    (
        [
            (header::CONTENT_TYPE.as_str(), "audio/wav".to_owned()),
            ("x-synthesis-ms", millis(start).to_string()),
            ("x-voice", voice),
        ],
        bytes,
    )
        .into_response()
}

async fn speak_upgrade(State(server): State<VoiceServer>, upgrade: WebSocketUpgrade) -> Response {
    upgrade
        .max_message_size(BODY_LIMIT)
        .max_frame_size(BODY_LIMIT)
        .on_upgrade(move |socket| speak_socket(server, socket))
}
async fn listen_upgrade(State(server): State<VoiceServer>, upgrade: WebSocketUpgrade) -> Response {
    upgrade
        .max_message_size(LISTEN_LIMIT)
        .max_frame_size(LISTEN_LIMIT)
        .on_upgrade(move |socket| listen_socket(server, socket))
}

#[derive(Clone)]
struct Writer(Arc<Mutex<SplitSink<WebSocket, Message>>>);

impl Writer {
    async fn send(&self, message: Message) -> bool {
        self.0.lock().await.send(message).await.is_ok()
    }
    async fn json(&self, value: Value) -> bool {
        self.send(Message::Text(value.to_string().into())).await
    }
    async fn error(&self, id: Option<&str>, error: &str) -> bool {
        self.json(match id {
            Some(id) => json!({"type":"error","id":id,"error":error}),
            None => json!({"type":"error","error":error}),
        })
        .await
    }
}

struct Reader {
    stream: SplitStream<WebSocket>,
    deadline: time::Instant,
    pinged: bool,
}

impl Reader {
    async fn next(&mut self, writer: &Writer) -> Option<Message> {
        loop {
            tokio::select! {
                message = self.stream.next() => {
                    let message = message?.ok()?;
                    self.deadline = time::Instant::now() + HEARTBEAT;
                    self.pinged = false;
                    match message {
                        Message::Close(_) => return None,
                        Message::Ping(_) | Message::Pong(_) => continue,
                        other => return Some(other),
                    }
                }
                () = time::sleep_until(self.deadline) => {
                    if self.pinged || !writer.send(Message::Ping(Default::default())).await { return None; }
                    self.pinged = true;
                    self.deadline = time::Instant::now() + HEARTBEAT / 2;
                }
            }
        }
    }
}

fn split(socket: WebSocket) -> (Writer, Reader) {
    let (sink, stream) = socket.split();
    (
        Writer(Arc::new(Mutex::new(sink))),
        Reader {
            stream,
            deadline: time::Instant::now() + HEARTBEAT,
            pinged: false,
        },
    )
}

struct Job {
    id: String,
    request: SpeakRequest,
    stop: CancellationToken,
}

type Pending = Arc<std::sync::Mutex<HashMap<String, CancellationToken>>>;

async fn speak_socket(server: VoiceServer, socket: WebSocket) {
    let (writer, mut reader) = split(socket);
    if !writer.json(json!({"type":"ready"})).await {
        return;
    }
    let (queue, jobs) = mpsc::unbounded_channel();
    let pending: Pending = Default::default();
    let worker = tokio::spawn(speak_worker(
        server.clone(),
        writer.clone(),
        jobs,
        pending.clone(),
    ));
    while let Some(message) = reader.next(&writer).await {
        let Message::Text(text) = message else {
            if !writer.error(None, "expected text frames").await {
                break;
            }
            continue;
        };
        let Ok(body) = serde_json::from_str::<Value>(text.as_str()) else {
            if !writer.error(None, "invalid JSON").await {
                break;
            }
            continue;
        };
        match body.get("type").and_then(Value::as_str) {
            Some("cancel") => {
                let target = body.get("id").filter(|v| !v.is_null());
                let pending = pending.lock().unwrap_or_else(|e| e.into_inner());
                for (id, stop) in &*pending {
                    if target.is_none_or(|v| v.as_str() == Some(id)) {
                        stop.cancel();
                    }
                }
            }
            Some("speak") => {
                let Some(id) = body
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| valid_id(id, 64))
                else {
                    if !writer.error(None, "invalid id").await {
                        break;
                    }
                    continue;
                };
                let duplicate = pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains_key(id);
                if duplicate {
                    if !writer.error(Some(id), "duplicate id").await {
                        break;
                    }
                    continue;
                }
                match server.request(&body) {
                    Ok(request) => {
                        let stop = CancellationToken::new();
                        pending
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(id.to_owned(), stop.clone());
                        if queue
                            .send(Job {
                                id: id.into(),
                                request,
                                stop,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(error) => {
                        if !writer.error(Some(id), &error.to_string()).await {
                            break;
                        }
                    }
                }
            }
            _ => {
                if !writer.error(None, "expected type speak or cancel").await {
                    break;
                }
            }
        }
    }
    for stop in pending.lock().unwrap_or_else(|e| e.into_inner()).values() {
        stop.cancel();
    }
    worker.abort();
}

async fn speak_worker(
    server: VoiceServer,
    writer: Writer,
    mut jobs: mpsc::UnboundedReceiver<Job>,
    pending: Pending,
) {
    while let Some(job) = jobs.recv().await {
        let id = job.id.clone();
        let terminal = if job.stop.is_cancelled() {
            json!({"type":"done","id":id,"cancelled":true,"firstChunkMs":null,"ms":0,"audioMs":0})
        } else if let Some(terminal) = produce(&server.0.tts, &writer, job).await {
            terminal
        } else {
            break;
        };
        pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        if !writer.json(terminal).await {
            break;
        }
    }
}

async fn produce(
    pool: &ThreadPool,
    writer: &Writer,
    Job { id, request, stop }: Job,
) -> Option<Value> {
    let start = Instant::now();
    let rate = request.engine.info.sample_rate;
    if !writer.json(json!({"type":"start","id":id,"engine":request.engine.info.id,"voice":request.voice_id(),"sampleRate":rate})).await { return None; }
    let (send, mut chunks) = mpsc::channel(4);
    let producer_stop = stop.clone();
    let mut prefix = [0; 65];
    prefix[0] = id.len() as u8;
    prefix[1..=id.len()].copy_from_slice(id.as_bytes());
    let prefix_len = id.len() + 1;
    let finished = dispatch(pool, move || {
        request.stream(&producer_stop, &mut |audio| {
            let mut frame = Vec::with_capacity(prefix_len + audio.len() * 2);
            frame.extend_from_slice(&prefix[..prefix_len]);
            append_pcm16(&mut frame, audio);
            send.blocking_send((audio.len(), frame)).is_ok()
        })
    });
    let mut first = None;
    let mut samples = 0usize;
    while let Some((count, frame)) = chunks.recv().await {
        if stop.is_cancelled() {
            continue;
        }
        first.get_or_insert_with(|| millis(start));
        samples += count;
        if !writer.send(Message::Binary(frame.into())).await {
            return None;
        }
    }
    Some(match finished.await {
        Ok(Ok(())) => {
            json!({"type":"done","id":id,"cancelled":stop.is_cancelled(),"firstChunkMs":first,
            "ms":millis(start),"audioMs":(samples as f64 * 1000. / f64::from(rate)).round_ties_even() as u64})
        }
        Ok(Err(error)) => json!({"type":"error","id":id,"error":error.to_string()}),
        Err(_) => json!({"type":"error","id":id,"error":"synthesis worker closed"}),
    })
}

async fn listen_socket(server: VoiceServer, socket: WebSocket) {
    let (writer, mut reader) = split(socket);
    let Ok(vad) = (server.0.vad)() else {
        return;
    };
    let mut segmenter = Segmenter::new(vad, SAMPLE_RATE, server.0.preroll_ms);
    if !writer
        .json(json!({"type":"ready","sampleRate":SAMPLE_RATE}))
        .await
    {
        return;
    }
    let mut decoding = FuturesOrdered::new();
    loop {
        tokio::select! {
            message = reader.next(&writer) => {
                match message {
                    Some(Message::Text(text)) => {
                        let Ok(body) = serde_json::from_str::<Value>(text.as_str()) else { break; };
                        if body.get("type").and_then(Value::as_str) == Some("reset") {
                            let was_speaking = segmenter.speaking();
                            segmenter.reset();
                            if was_speaking && !writer.json(json!({"type":"speech_end"})).await { break; }
                        }
                    }
                    Some(Message::Binary(pcm)) => {
                        let Ok(events) = pcm16_to_float(&pcm).and_then(|samples| segmenter.accept(&samples)) else { break; };
                        for event in events {
                            match event {
                                SegmenterEvent::SpeechStart => if !writer.json(json!({"type":"speech_start"})).await { return; },
                                SegmenterEvent::SpeechEnd(samples) => {
                                    if !writer.json(json!({"type":"speech_end"})).await { return; }
                                    let started = Instant::now();
                                    let recognizer = server.0.recognizer.clone();
                                    let audio = dispatch(&server.0.stt, move || recognizer.transcribe(&samples));
                                    decoding.push_back(async move { (audio.await, started) });
                                }
                            }
                        }
                    }
                    _ => break,
                }
            }
            Some((decoded, started)) = decoding.next(), if !decoding.is_empty() => {
                match decoded {
                    Ok(Ok(text)) => {
                        if !writer.json(json!({"type":"final","text":text.trim_matches(python_space),"ms":millis(started)})).await { break; }
                    }
                    Ok(Err(error)) => eprintln!("voice transcription failed: {error}"),
                    Err(_) => eprintln!("voice transcription worker closed"),
                }
            }
        }
    }
}

fn dispatch<T: Send + 'static>(
    pool: &ThreadPool,
    f: impl FnOnce() -> T + Send + 'static,
) -> oneshot::Receiver<T> {
    let (send, rx) = oneshot::channel();
    pool.spawn_fifo(move || {
        let _ = send.send(f());
    });
    rx
}

fn millis(start: Instant) -> u64 {
    (start.elapsed().as_secs_f64() * 1000.).round_ties_even() as u64
}
