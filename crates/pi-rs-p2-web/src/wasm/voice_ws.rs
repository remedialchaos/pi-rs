//! Live WebAudio + `/voice/room` WebSocket streaming client (mic capture, VAD, TTS playback).

use crate::state::voice_audio::{
    AGENT_AUDIO, DEFAULT_PAUSE_SECS, HUMAN_RATE, RoomEvent, VadDetector, VadTransition,
    downsample_f32_to_pcm16le, parse_room_frame, pcm16le_to_f32, voice_room_ws_url,
};
use crate::ui::terminal::VoiceState;
use futures_channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures_util::{FutureExt, SinkExt, StreamExt};
use gloo_net::websocket::{Message, futures::WebSocket};
use leptos::prelude::*;
use serde_json::json;
use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

#[derive(Clone, Debug)]
pub enum VoiceWsAction {
    MicPcm(Vec<u8>),
    SetVad(f64),
    CancelTts,
    Stop,
}

#[derive(Clone, Copy)]
pub struct VoiceRuntime {
    pub state: RwSignal<VoiceState>,
    pub muted: RwSignal<bool>,
    pub pause_secs: RwSignal<f64>,
    pub interim: RwSignal<String>,
    pub final_text: RwSignal<String>,
    pub tx: StoredValue<Option<UnboundedSender<VoiceWsAction>>>,
}

impl VoiceRuntime {
    pub fn new() -> Self {
        Self {
            state: RwSignal::new(VoiceState::Idle),
            muted: RwSignal::new(false),
            pause_secs: RwSignal::new(DEFAULT_PAUSE_SECS),
            interim: RwSignal::new(String::new()),
            final_text: RwSignal::new(String::new()),
            tx: StoredValue::new(None),
        }
    }

    pub fn send(&self, action: VoiceWsAction) {
        if let Some(sender) = self.tx.get_value() {
            let _ = sender.unbounded_send(action);
        }
    }

    pub fn stop(&self) {
        self.send(VoiceWsAction::Stop);
        self.tx.set_value(None);
        self.state.set(VoiceState::Idle);
    }
}

pub fn start_voice_session(runtime: VoiceRuntime, session_id: String, conversation: u64) {
    runtime.stop();
    let loc = window().location();
    let scheme = loc.protocol().unwrap_or_else(|_| "http:".into());
    let host = loc.host().unwrap_or_default();
    let ws_url = voice_room_ws_url(&scheme, &host, &session_id, conversation);

    let (tx, mut rx): (
        UnboundedSender<VoiceWsAction>,
        UnboundedReceiver<VoiceWsAction>,
    ) = unbounded();
    runtime.tx.set_value(Some(tx.clone()));
    runtime.state.set(VoiceState::Listening);

    wasm_bindgen_futures::spawn_local(async move {
        let Ok(mut ws) = WebSocket::open(&ws_url) else {
            runtime.state.set(VoiceState::Idle);
            return;
        };

        let audio_ctx = web_sys::AudioContext::new().ok();
        let next_play_time = Rc::new(RefCell::new(0.0f64));
        let tts_sample_rate = Rc::new(RefCell::new(24_000u32));

        // Start microphone capture via navigator.mediaDevices.getUserMedia({ audio: true }).
        if let Some(ref ctx) = audio_ctx {
            start_mic_capture(ctx, runtime, tx.clone());
        }

        loop {
            futures_util::select! {
                msg = ws.next().fuse() => match msg {
                    Some(Ok(Message::Text(txt))) => {
                        if let Ok(ev) = serde_json::from_str::<RoomEvent>(&txt) {
                            match ev {
                                RoomEvent::SpeechStart => runtime.state.set(VoiceState::Listening),
                                RoomEvent::SpeechEnd => runtime.state.set(VoiceState::Processing),
                                RoomEvent::TtsStart { sample_rate, .. } => {
                                    *tts_sample_rate.borrow_mut() = sample_rate.max(8000);
                                    runtime.state.set(VoiceState::Speaking);
                                }
                                RoomEvent::TtsDone { .. } | RoomEvent::TtsError { .. } => {
                                    runtime.state.set(VoiceState::Listening);
                                }
                                RoomEvent::Interim { text, .. } => runtime.interim.set(text),
                                RoomEvent::Final { text, .. } => {
                                    runtime.interim.set(String::new());
                                    runtime.final_text.set(text);
                                }
                                _ => {}
                            }
                        }
                    }
                    Some(Ok(Message::Bytes(bytes))) => {
                        if let Some(frame) = parse_room_frame(&bytes)
                            && frame.kind == AGENT_AUDIO
                            && let Some(ref ctx) = audio_ctx
                        {
                            play_pcm_chunk(ctx, &frame.pcm, *tts_sample_rate.borrow(), &next_play_time);
                        }
                    }
                    Some(Err(_)) | None => {
                        runtime.state.set(VoiceState::Idle);
                        break;
                    }
                },
                act = rx.next().fuse() => match act {
                    Some(VoiceWsAction::MicPcm(pcm)) => {
                        if !runtime.muted.get_untracked()
                            && ws.send(Message::Bytes(pcm)).await.is_err()
                        {
                            break;
                        }
                    }
                    Some(VoiceWsAction::SetVad(silence)) => {
                        let cmd = json!({ "type": "vad", "silence": silence }).to_string();
                        if ws.send(Message::Text(cmd)).await.is_err() {
                            break;
                        }
                    }
                    Some(VoiceWsAction::CancelTts) => {
                        let cmd = json!({ "type": "cancel" }).to_string();
                        if ws.send(Message::Text(cmd)).await.is_err() {
                            break;
                        }
                    }
                    Some(VoiceWsAction::Stop) | None => {
                        let _ = ws.close(None, None);
                        if let Some(ref ctx) = audio_ctx {
                            let _ = ctx.close();
                        }
                        runtime.state.set(VoiceState::Idle);
                        break;
                    }
                }
            }
        }
    });
}

fn start_mic_capture(
    ctx: &web_sys::AudioContext,
    runtime: VoiceRuntime,
    tx: UnboundedSender<VoiceWsAction>,
) {
    let ctx_clone = ctx.clone();
    wasm_bindgen_futures::spawn_local(async move {
        let Ok(media_devices) = js_sys::Reflect::get(&window().navigator(), &"mediaDevices".into())
        else {
            return;
        };
        let Ok(get_user_media) = js_sys::Reflect::get(&media_devices, &"getUserMedia".into())
            .and_then(|v| v.dyn_into::<js_sys::Function>())
        else {
            return;
        };
        let constraints = js_sys::Object::new();
        let _ = js_sys::Reflect::set(&constraints, &"audio".into(), &JsValue::TRUE);
        let Ok(promise_val) = get_user_media.call1(&media_devices, &constraints) else {
            return;
        };
        let Ok(promise) = promise_val.dyn_into::<js_sys::Promise>() else {
            return;
        };
        let Ok(stream) = wasm_bindgen_futures::JsFuture::from(promise).await else {
            return;
        };

        let Ok(src_fn) = js_sys::Reflect::get(&ctx_clone, &"createMediaStreamSource".into())
            .and_then(|v| v.dyn_into::<js_sys::Function>())
        else {
            return;
        };
        let Ok(source_node) = src_fn.call1(&ctx_clone, &stream) else {
            return;
        };
        let Ok(proc_fn) = js_sys::Reflect::get(&ctx_clone, &"createScriptProcessor".into())
            .and_then(|v| v.dyn_into::<js_sys::Function>())
        else {
            return;
        };
        let Ok(proc_node) = proc_fn.call3(
            &ctx_clone,
            &JsValue::from_f64(4096.0),
            &JsValue::from_f64(1.0),
            &JsValue::from_f64(1.0),
        ) else {
            return;
        };

        let sample_rate = js_sys::Reflect::get(&ctx_clone, &"sampleRate".into())
            .ok()
            .and_then(|v| v.as_f64())
            .unwrap_or(48_000.0) as u32;
        let vad = Rc::new(RefCell::new(VadDetector::default()));
        let vad_ref = vad.clone();
        let on_audio = Closure::<dyn FnMut(JsValue)>::new(move |ev: JsValue| {
            if runtime.muted.get_untracked() {
                return;
            }
            let Ok(buf) = js_sys::Reflect::get(&ev, &"inputBuffer".into()) else {
                return;
            };
            let Ok(get_ch) = js_sys::Reflect::get(&buf, &"getChannelData".into())
                .and_then(|v| v.dyn_into::<js_sys::Function>())
            else {
                return;
            };
            let Ok(ch0) = get_ch.call1(&buf, &JsValue::from_f64(0.0)) else {
                return;
            };
            let samples = js_sys::Float32Array::new(&ch0).to_vec();
            let frame_ms = ((samples.len() as f64 / f64::from(sample_rate.max(1))) * 1000.0) as u64;
            let mut detector = vad_ref.borrow_mut();
            detector.set_silence_secs(runtime.pause_secs.get_untracked());
            match detector.process_frame(&samples, frame_ms) {
                VadTransition::SpeechStart => runtime.state.set(VoiceState::Listening),
                VadTransition::SpeechEnd => runtime.state.set(VoiceState::Processing),
                VadTransition::None => {}
            }
            if detector.is_speaking() {
                let pcm = downsample_f32_to_pcm16le(&samples, sample_rate, HUMAN_RATE);
                let _ = tx.unbounded_send(VoiceWsAction::MicPcm(pcm));
            }
        });
        let _ = js_sys::Reflect::set(&proc_node, &"onaudioprocess".into(), on_audio.as_ref());
        on_audio.forget();

        if let Ok(connect_src) = js_sys::Reflect::get(&source_node, &"connect".into())
            .and_then(|v| v.dyn_into::<js_sys::Function>())
        {
            let _ = connect_src.call1(&source_node, &proc_node);
        }
        if let (Ok(connect_proc), Ok(dest)) = (
            js_sys::Reflect::get(&proc_node, &"connect".into())
                .and_then(|v| v.dyn_into::<js_sys::Function>()),
            js_sys::Reflect::get(&ctx_clone, &"destination".into()),
        ) {
            let _ = connect_proc.call1(&proc_node, &dest);
        }
    });
}

fn play_pcm_chunk(
    ctx: &web_sys::AudioContext,
    pcm: &[u8],
    sample_rate: u32,
    next_play_time: &Rc<RefCell<f64>>,
) {
    let samples = pcm16le_to_f32(pcm);
    if samples.is_empty() {
        return;
    }
    let Ok(create_buf) = js_sys::Reflect::get(ctx, &"createBuffer".into())
        .and_then(|v| v.dyn_into::<js_sys::Function>())
    else {
        return;
    };
    let Ok(audio_buf) = create_buf.call3(
        ctx,
        &JsValue::from_f64(1.0),
        &JsValue::from_f64(samples.len() as f64),
        &JsValue::from_f64(f64::from(sample_rate.max(8000))),
    ) else {
        return;
    };
    if let Ok(copy_fn) = js_sys::Reflect::get(&audio_buf, &"copyToChannel".into())
        .and_then(|v| v.dyn_into::<js_sys::Function>())
    {
        let arr = js_sys::Float32Array::from(samples.as_slice());
        let _ = copy_fn.call2(&audio_buf, &arr, &JsValue::from_f64(0.0));
    }
    let Ok(create_src) = js_sys::Reflect::get(ctx, &"createBufferSource".into())
        .and_then(|v| v.dyn_into::<js_sys::Function>())
    else {
        return;
    };
    let Ok(src) = create_src.call0(ctx) else {
        return;
    };
    let _ = js_sys::Reflect::set(&src, &"buffer".into(), &audio_buf);
    if let (Ok(connect_fn), Ok(dest)) = (
        js_sys::Reflect::get(&src, &"connect".into())
            .and_then(|v| v.dyn_into::<js_sys::Function>()),
        js_sys::Reflect::get(ctx, &"destination".into()),
    ) {
        let _ = connect_fn.call1(&src, &dest);
    }
    let now = js_sys::Reflect::get(ctx, &"currentTime".into())
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let mut next = next_play_time.borrow_mut();
    let start_at = next.max(now);
    let dur = samples.len() as f64 / f64::from(sample_rate.max(8000));
    *next = start_at + dur;
    if let Ok(start_fn) =
        js_sys::Reflect::get(&src, &"start".into()).and_then(|v| v.dyn_into::<js_sys::Function>())
    {
        let _ = start_fn.call1(&src, &JsValue::from_f64(start_at));
    }
}

/// Fetches a sample TTS clip from `POST /voice/sample` and plays it on `AudioContext`.
pub fn play_voice_sample(engine: Option<String>, voice: Option<String>, speed: f64) {
    wasm_bindgen_futures::spawn_local(async move {
        let mut body = serde_json::Map::new();
        body.insert("speed".into(), json!(speed.clamp(0.5, 2.0)));
        if let Some(e) = engine.filter(|s| !s.is_empty()) {
            body.insert("engine".into(), json!(e));
        }
        if let Some(v) = voice.filter(|s| !s.is_empty()) {
            body.insert("voice".into(), json!(v));
        }
        let Ok(req) =
            gloo_net::http::Request::post("/voice/sample").json(&serde_json::Value::Object(body))
        else {
            return;
        };
        let Ok(resp) = req.send().await else {
            return;
        };
        if !resp.ok() {
            return;
        }
        let Ok(bytes) = resp.binary().await else {
            return;
        };
        if let Ok(ctx) = web_sys::AudioContext::new() {
            let next = Rc::new(RefCell::new(0.0f64));
            play_pcm_chunk(&ctx, &bytes, 24_000, &next);
        }
    });
}
