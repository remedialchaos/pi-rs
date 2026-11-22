#![deny(unsafe_code)]

mod audio;
mod engine;
mod error;
pub mod models;
mod segmenter;
mod server;
pub mod text;

pub use audio::{pcm16, pcm16_to_float, wav};
pub use engine::{
    Engine, EngineInfo, SentenceEngine, SentenceSynthesizer, SttRecognizer, TtsEngine, Voice,
};
pub use error::{Error, Result};
pub use segmenter::{
    SAMPLE_RATE, Segmenter, SegmenterEvent, SpeechSegment, VadOutput, VoiceActivityDetector, WINDOW,
};
pub use server::{VoiceServer, router};
