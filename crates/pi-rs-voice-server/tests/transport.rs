#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex, mpsc as thread};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use pi_rs_voice_server::{
    Engine, EngineInfo, Error, Result, SAMPLE_RATE, SpeechSegment, SttRecognizer, TtsEngine,
    VadOutput, Voice, VoiceActivityDetector, VoiceServer, WINDOW, router,
};
use serde_json::{Value, json};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::Notify,
    task::JoinHandle,
    time::timeout,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};
use tokio_util::sync::CancellationToken;

type Test<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
const TOKEN: &str = "local-token";
const WAIT: Duration = Duration::from_secs(3);

#[derive(Default)]
struct MockTts {
    gate: Option<thread::Receiver<()>>,
}
impl TtsEngine for MockTts {
    fn stream(
        &mut self,
        text: &str,
        voice: &str,
        speed: f32,
        stop: &CancellationToken,
        emit: &mut dyn FnMut(&[f32]) -> bool,
    ) -> Result<()> {
        if text == "fail" {
            return Err(Error::Model("synthesis exploded".into()));
        }
        if text == "inspect" {
            return if emit(&[if voice == "F2" { 0.75 } else { 0.25 }, speed / 2.]) {
                Ok(())
            } else {
                Err(Error::Model("sink closed".into()))
            };
        }
        if !emit(&[1., -1., 0.5]) {
            return Ok(());
        }
        if text == "hold" {
            let gate = self
                .gate
                .as_ref()
                .ok_or_else(|| Error::Model("missing test gate".into()))?;
            gate.recv_timeout(WAIT)
                .map_err(|e| Error::Model(e.to_string()))?;
            if !stop.is_cancelled() {
                emit(&[0.75]);
            }
        }
        Ok(())
    }
}

struct MockStt {
    first: Option<Mutex<thread::Receiver<()>>>,
    second_completed: Arc<Notify>,
}
impl SttRecognizer for MockStt {
    fn transcribe(&self, samples: &[f32]) -> Result<String> {
        match samples.first().copied() {
            Some(0.25) => {
                if let Some(first) = &self.first {
                    first
                        .lock()
                        .map_err(|_| Error::Model("gate poisoned".into()))?
                        .recv_timeout(WAIT)
                        .map_err(|e| Error::Model(e.to_string()))?;
                }
                Ok("  first  ".into())
            }
            Some(0.5) => {
                self.second_completed.notify_one();
                Ok("second".into())
            }
            _ => Err(Error::Model("unexpected PCM samples".into())),
        }
    }
}

#[derive(Default)]
struct MockVad {
    start: usize,
    at: usize,
    active: Vec<f32>,
}
impl VoiceActivityDetector for MockVad {
    fn accept(&mut self, frame: &[f32; WINDOW]) -> Result<VadOutput> {
        let start = self.at;
        self.at += WINDOW;
        if frame[0] > 0. {
            if self.active.is_empty() {
                self.start = start;
            }
            self.active.extend(frame);
            return Ok(VadOutput {
                speaking: true,
                segment: None,
            });
        }
        let segment = (!self.active.is_empty()).then(|| SpeechSegment {
            start: self.start,
            samples: std::mem::take(&mut self.active),
        });
        Ok(VadOutput {
            speaking: false,
            segment,
        })
    }
    fn reset(&mut self) {
        *self = Self::default();
    }
}

struct Server {
    http: String,
    ws: String,
    task: JoinHandle<std::io::Result<()>>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn start(
        engine: &str,
        voice: Option<&str>,
        gate: Option<thread::Receiver<()>>,
        stt: Option<MockStt>,
    ) -> Test<Self> {
        let make_voice = |id: &str, name: &str| Voice {
            id: id.into(),
            name: name.into(),
        };
        let supertonic = Engine::new(
            EngineInfo {
                id: "supertonic".into(),
                name: "Supertonic 3".into(),
                streaming: false,
                sample_rate: 44_100,
                default_speed: 1.05,
                default_voice: "F1".into(),
                voices: vec![
                    make_voice("M1", "Male 1"),
                    make_voice("F1", "Female 1"),
                    make_voice("F2", "Female 2"),
                ],
            },
            MockTts { gate },
        )?;
        let pocket = Engine::new(
            EngineInfo {
                id: "pocket".into(),
                name: "Pocket TTS (Kyutai)".into(),
                streaming: true,
                sample_rate: 24_000,
                default_speed: 1.,
                default_voice: "alba".into(),
                voices: vec![make_voice("vera", "Vera"), make_voice("alba", "Alba")],
            },
            MockTts::default(),
        )?;
        let stt = stt.unwrap_or_else(|| MockStt {
            first: None,
            second_completed: Arc::new(Notify::new()),
        });
        let configured = VoiceServer::new(
            TOKEN,
            vec![supertonic, pocket],
            engine,
            voice,
            || Ok(Box::<MockVad>::default()),
            0.,
            Arc::new(stt),
        )?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let task = tokio::spawn(async { axum::serve(listener, router(configured)).await });
        Ok(Self {
            http: format!("http://{address}"),
            ws: format!("ws://{address}"),
            task,
        })
    }
    async fn connect(&self, path: &str) -> Test<Socket> {
        let mut req = format!("{}{path}", self.ws).into_client_request()?;
        req.headers_mut()
            .insert("authorization", format!("Bearer {TOKEN}").parse()?);
        Ok(timeout(WAIT, connect_async(req)).await??.0)
    }
}

async fn next(socket: &mut Socket) -> Test<Message> {
    Ok(timeout(WAIT, socket.next())
        .await?
        .ok_or("voice socket closed")??)
}
async fn next_json(socket: &mut Socket) -> Test<Value> {
    Ok(serde_json::from_str(next(socket).await?.to_text()?)?)
}
async fn send(socket: &mut Socket, payload: Value) -> Test {
    socket
        .send(Message::Text(payload.to_string().into()))
        .await?;
    Ok(())
}
fn req(id: &str, text: &str) -> Value {
    json!({"type":"speak","id":id,"text":text})
}
fn client() -> Test<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .no_proxy()
        .timeout(WAIT)
        .build()?)
}

#[tokio::test]
async fn http_auth_is_exact_with_public_health_and_websocket_rejection() -> Test {
    let server = Server::start("supertonic", None, None, None).await?;
    let client = client()?;
    let health = client.get(format!("{}/health", server.http)).send().await?;
    assert_eq!(health.status(), 200);
    assert_eq!(health.json::<Value>().await?, json!({"ok":true}));
    for path in ["/voices", "/speak", "/speak/stream", "/listen", "/unknown"] {
        let resp = client.get(format!("{}{path}", server.http)).send().await?;
        assert_eq!(resp.status(), 401);
        assert_eq!(resp.json::<Value>().await?, json!({"error":"unauthorized"}));
    }
    for token in ["Bearer local-tokeN", "bearer local-token", "Bearer local"] {
        let resp = client
            .get(format!("{}/voices", server.http))
            .header("Authorization", token)
            .send()
            .await?;
        assert_eq!(resp.status(), 401);
    }
    assert_eq!(
        client
            .get(format!("{}/voices", server.http))
            .bearer_auth(TOKEN)
            .send()
            .await?
            .status(),
        200
    );
    let error = timeout(WAIT, connect_async(format!("{}/listen", server.ws))).await?;
    assert!(
        matches!(error, Err(tokio_tungstenite::tungstenite::Error::Http(response)) if response.status() == 401)
    );
    Ok(())
}

#[tokio::test]
async fn catalog_preserves_engine_order_with_effective_default_voice_first() -> Test {
    let client = client()?;
    for (engine, voice, expected_engine, expected_voice) in [
        ("pocket", Some("vera"), "pocket", "pocket-vera"),
        ("not-installed", Some("F2"), "supertonic", "supertonic-F2"),
        ("supertonic", Some("nope"), "supertonic", "supertonic-F1"),
    ] {
        let server = Server::start(engine, voice, None, None).await?;
        let catalog = client
            .get(format!("{}/voices", server.http))
            .bearer_auth(TOKEN)
            .send()
            .await?
            .json::<Value>()
            .await?;
        assert_eq!(
            catalog["default"],
            json!({"engine":expected_engine,"voice":expected_voice})
        );
        assert_eq!(catalog["engines"][0]["id"], "supertonic");
        assert_eq!(catalog["engines"][0]["sampleRate"], 44100);
        assert_eq!(catalog["engines"][1]["streaming"], true);
        let index = usize::from(expected_engine == "pocket");
        assert_eq!(catalog["engines"][index]["voices"][0]["id"], expected_voice);
        assert_eq!(
            catalog["engines"][1 - index]["voices"][0]["id"],
            if index == 1 {
                "supertonic-F1"
            } else {
                "pocket-alba"
            }
        );
    }
    Ok(())
}

#[tokio::test]
async fn speak_returns_pcm_wav_headers_and_per_engine_speed_defaults() -> Test {
    let server = Server::start("supertonic", Some("F2"), None, None).await?;
    let client = client()?;
    let url = format!("{}/speak", server.http);
    for (body, rate, voice, samples) in [
        (
            json!({"text":" inspect "}),
            44_100u32,
            "supertonic-F2",
            [24_575i16, 17_202],
        ),
        (
            json!({"text":"inspect","voice":"pocket-alba","speed":"1.5"}),
            24_000,
            "pocket-alba",
            [8191, 24575],
        ),
        (
            json!({"text":"inspect","engine":"pocket"}),
            24_000,
            "pocket-alba",
            [8191, 16383],
        ),
    ] {
        let resp = client
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&body)
            .send()
            .await?;
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers()["content-type"], "audio/wav");
        assert_eq!(resp.headers()["x-voice"], voice);
        resp.headers()["x-synthesis-ms"].to_str()?.parse::<u64>()?;
        let bytes = resp.bytes().await?;
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(&bytes[4..8], &40u32.to_le_bytes());
        assert_eq!(&bytes[8..24], b"WAVEfmt \x10\0\0\0\x01\0\x01\0");
        assert_eq!(&bytes[24..28], &rate.to_le_bytes());
        assert_eq!(&bytes[28..32], &(rate * 2).to_le_bytes());
        assert_eq!(&bytes[32..44], b"\x02\0\x10\0data\x04\0\0\0");
        assert_eq!(bytes[44..], samples.map(i16::to_le_bytes).concat());
    }
    Ok(())
}

#[tokio::test]
async fn invalid_http_inputs_use_exact_validation_messages_and_body_limit() -> Test {
    let server = Server::start("supertonic", None, None, None).await?;
    let client = client()?;
    let url = format!("{}/speak", server.http);
    for (body, error) in [
        (json!({}), "expected {text, voice?, engine?, speed?}"),
        (
            json!({"text":false}),
            "expected {text, voice?, engine?, speed?}",
        ),
        (json!({"text":" \n\u{001c}"}), "invalid text"),
        (json!({"text":"é".repeat(2001)}), "invalid text"),
        (
            json!({"text":"yes","engine":"unknown"}),
            "unknown engine unknown",
        ),
        (
            json!({"text":"yes","voice":"pocket-nope"}),
            "unknown voice pocket-nope",
        ),
        (
            json!({"text":"yes","voice":"pocket-alba","engine":"supertonic"}),
            "voice does not belong to engine",
        ),
        (json!({"text":"yes","speed":0.49}), "invalid speed"),
        (json!({"text":"yes","speed":2.01}), "invalid speed"),
        (json!({"text":"yes","speed":"NaN"}), "invalid speed"),
        (json!({"text":"yes","speed":{}}), "invalid speed"),
    ] {
        let response = client
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&body)
            .send()
            .await?;
        assert_eq!(response.status(), 400);
        assert_eq!(response.json::<Value>().await?, json!({"error":error}));
    }
    let response = client
        .post(&url)
        .bearer_auth(TOKEN)
        .json(&json!({"text":"x","padding":"x".repeat(65536)}))
        .send()
        .await?;
    assert_eq!(response.status(), 413);
    // The body says what went wrong, so a client does not have to guess from the status.
    assert_eq!(
        response.json::<Value>().await?,
        json!({"error":"request body too large"})
    );
    Ok(())
}

#[tokio::test]
async fn a_failed_synthesis_returns_an_error_body() -> Test {
    let server = Server::start("supertonic", None, None, None).await?;
    let client = client()?;
    let response = client
        .post(format!("{}/speak", server.http))
        .bearer_auth(TOKEN)
        .json(&json!({"text":"fail"}))
        .send()
        .await?;
    assert_eq!(response.status(), 500);
    // The body says what went wrong, so a client does not have to guess from the status.
    assert_eq!(
        response.json::<Value>().await?,
        json!({"error":"synthesis exploded"})
    );
    Ok(())
}

#[tokio::test]
async fn speak_socket_serializes_jobs_and_preserves_binary_frame_offsets() -> Test {
    let server = Server::start("supertonic", None, None, None).await?;
    let mut socket = server.connect("/speak/stream").await?;
    assert_eq!(next_json(&mut socket).await?, json!({"type":"ready"}));
    send(&mut socket, req("ab", "one")).await?;
    send(&mut socket, req("odd", "two")).await?;
    for id in ["ab", "odd"] {
        assert_eq!(
            next_json(&mut socket).await?,
            json!({"type":"start","id":id,"engine":"supertonic","voice":"supertonic-F1","sampleRate":44100})
        );
        let Message::Binary(audio) = next(&mut socket).await? else {
            return Err("expected binary audio".into());
        };
        assert_eq!(audio[0] as usize, id.len());
        assert_eq!(&audio[1..=id.len()], id.as_bytes());
        assert_eq!(&audio[1 + id.len()..], &[0xff, 0x7f, 1, 0x80, 0xff, 0x3f]);
        let done = next_json(&mut socket).await?;
        assert_eq!(done["type"], "done");
        assert_eq!(done["id"], id);
        assert_eq!(done["cancelled"], false);
        assert!(done["firstChunkMs"].is_u64() && done["ms"].is_u64());
        assert_eq!(done["audioMs"], 0);
    }
    Ok(())
}

#[tokio::test]
async fn speak_socket_rejects_bad_frames_and_reports_synthesis_failure() -> Test {
    let server = Server::start("supertonic", None, None, None).await?;
    let mut socket = server.connect("/speak/stream").await?;
    next_json(&mut socket).await?;
    for (frame, error) in [
        (Message::Binary(vec![1].into()), "expected text frames"),
        (Message::Text("{".into()), "invalid JSON"),
        (Message::Text("[]".into()), "expected type speak or cancel"),
        (
            Message::Text(req("bad id", "hi").to_string().into()),
            "invalid id",
        ),
        (
            Message::Text(req(&"A".repeat(65), "hi").to_string().into()),
            "invalid id",
        ),
    ] {
        socket.send(frame).await?;
        assert_eq!(
            next_json(&mut socket).await?,
            json!({"type":"error","error":error})
        );
    }
    send(&mut socket, req("fail", "fail")).await?;
    assert_eq!(next_json(&mut socket).await?["type"], "start");
    assert_eq!(
        next_json(&mut socket).await?,
        json!({"type":"error","id":"fail","error":"synthesis exploded"})
    );
    send(&mut socket, req("fail", "works after error")).await?;
    assert_eq!(next_json(&mut socket).await?["type"], "start");
    assert!(matches!(next(&mut socket).await?, Message::Binary(_)));
    assert_eq!(next_json(&mut socket).await?["cancelled"], false);
    Ok(())
}

#[tokio::test]
async fn speak_socket_cancels_queued_and_running_jobs_with_target_or_all() -> Test {
    for cancel_all in [false, true] {
        let (release, gate) = thread::sync_channel(1);
        let server = Server::start("supertonic", None, Some(gate), None).await?;
        let mut socket = server.connect("/speak/stream").await?;
        next_json(&mut socket).await?;
        send(&mut socket, req("running", "hold")).await?;
        assert_eq!(next_json(&mut socket).await?["id"], "running");
        assert!(matches!(next(&mut socket).await?, Message::Binary(_)));
        send(&mut socket, req("waiting", "second")).await?;
        send(&mut socket, req("waiting", "duplicate")).await?;
        assert_eq!(
            next_json(&mut socket).await?,
            json!({"type":"error","id":"waiting","error":"duplicate id"})
        );
        if cancel_all {
            send(&mut socket, json!({"type":"cancel"})).await?;
        } else {
            send(&mut socket, json!({"type":"cancel","id":"waiting"})).await?;
            send(&mut socket, json!({"type":"cancel","id":"running"})).await?;
        }
        send(&mut socket, json!({"type":"barrier"})).await?;
        assert_eq!(
            next_json(&mut socket).await?["error"],
            "expected type speak or cancel"
        );
        release.send(())?;
        let running = next_json(&mut socket).await?;
        assert_eq!(running["id"], "running");
        assert_eq!(running["cancelled"], true);
        assert!(running["firstChunkMs"].is_u64());
        assert_eq!(
            next_json(&mut socket).await?,
            json!({"type":"done","id":"waiting","cancelled":true,"firstChunkMs":null,"ms":0,"audioMs":0})
        );
    }
    Ok(())
}

fn pcm(value: i16) -> Message {
    Message::Binary(value.to_le_bytes().repeat(WINDOW).into())
}

#[tokio::test]
async fn listen_decodes_in_parallel_but_delivers_finals_in_utterance_order() -> Test {
    let (release, gate) = thread::sync_channel(1);
    let second = Arc::new(Notify::new());
    let stt = MockStt {
        first: Some(Mutex::new(gate)),
        second_completed: second.clone(),
    };
    let server = Server::start("supertonic", None, None, Some(stt)).await?;
    let mut socket = server.connect("/listen").await?;
    assert_eq!(
        next_json(&mut socket).await?,
        json!({"type":"ready","sampleRate":SAMPLE_RATE})
    );
    for frame in [pcm(8192), pcm(0), pcm(16384), pcm(0)] {
        socket.send(frame).await?;
    }
    for kind in ["speech_start", "speech_end", "speech_start", "speech_end"] {
        assert_eq!(next_json(&mut socket).await?["type"], kind);
    }
    timeout(WAIT, second.notified()).await?;
    release.send(())?;
    for text in ["first", "second"] {
        let value = next_json(&mut socket).await?;
        assert_eq!(value["type"], "final");
        assert_eq!(value["text"], text);
        assert!(value["ms"].is_u64());
    }
    Ok(())
}

#[tokio::test]
async fn listen_reset_only_reports_active_end_and_discards_partial_utterance() -> Test {
    let server = Server::start("supertonic", None, None, None).await?;
    let mut socket = server.connect("/listen").await?;
    next_json(&mut socket).await?;
    send(&mut socket, json!({"type":"reset"})).await?;
    socket.send(pcm(8192)).await?;
    assert_eq!(
        next_json(&mut socket).await?,
        json!({"type":"speech_start"})
    );
    send(&mut socket, json!({"type":"reset"})).await?;
    assert_eq!(next_json(&mut socket).await?, json!({"type":"speech_end"}));
    socket.send(pcm(16384)).await?;
    socket.send(pcm(0)).await?;
    assert_eq!(next_json(&mut socket).await?["type"], "speech_start");
    assert_eq!(next_json(&mut socket).await?["type"], "speech_end");
    assert_eq!(next_json(&mut socket).await?["text"], "second");
    Ok(())
}
