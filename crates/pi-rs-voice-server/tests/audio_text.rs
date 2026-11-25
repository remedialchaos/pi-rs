#![forbid(unsafe_code)]

use pi_rs_voice_server::{
    Error, Result, SAMPLE_RATE, Segmenter, SegmenterEvent, SentenceEngine, SentenceSynthesizer,
    SpeechSegment, TtsEngine, VadOutput, VoiceActivityDetector, WINDOW, pcm16, pcm16_to_float,
    text::{UnicodeProcessor, chunk_text, get_latent_mask, length_to_mask, preprocess, sentences},
};
use std::collections::VecDeque;
use tokio_util::sync::CancellationToken;

#[test]
fn unicode_preprocessing_and_sentence_groups_match_the_python_oracle() -> Result<()> {
    for (input, output) in [
        (
            "  Café 🚀 — ‘Hi’  ! e.g., @ here / now #  ",
            "<en>Cafe\u{301} -'Hi' ! for example, at here now.</en>",
        ),
        ("a‑b _ © ♥ [x] → \"\"y\"\"", "<en>a‐b x \"y\"</en>"),
        ("A\u{1c}B", "<en>A B.</en>"),
        ("””", "<en>\"</en>"),
        ("", "<en>.</en>"),
        ("a’ ’b", "<en>a'b.</en>"),
    ] {
        assert_eq!(preprocess(input, "en")?, output);
    }
    assert_eq!(preprocess("Voilà", "fr")?, "<fr>Voila\u{300}.</fr>");
    assert_eq!(preprocess("🚀🙿", "en")?, "<en>🙿.</en>");
    assert!(
        matches!(preprocess("bad", "xx"), Err(Error::Request(e)) if e == "Invalid language: xx")
    );
    let input = "Mr. Gray met Dr. Fox, Ph.D. Today he left. E. B. White waited. e.g. arrows! i.e. signs.\n \nSecond paragraph. Three more.";
    assert_eq!(
        sentences(input)?,
        [
            "Mr. Gray met Dr. Fox, Ph.D. Today he left.",
            "E. B. White waited.",
            "e.g. arrows!",
            "i.e. signs.",
            "Second paragraph.",
            "Three more."
        ]
    );
    assert_eq!(
        chunk_text(input, 38)?,
        [
            "Mr. Gray met Dr. Fox, Ph.D. Today he left.",
            "E. B. White waited. e.g. arrows!",
            "i.e. signs.",
            "Second paragraph. Three more."
        ]
    );
    assert_eq!(
        chunk_text("你好。 Still one paragraph.\n\n  Another.", 200)?,
        ["你好。 Still one paragraph.", "Another."]
    );
    Ok(())
}

#[test]
fn unicode_indices_and_tensor_masks_keep_batch_padding_and_ceiling_lengths() -> Result<()> {
    let index = UnicodeProcessor::new((0..=65535).map(|x| (x * 3) % 997).collect());
    let expected = "<en>Hi.</en>"
        .bytes()
        .map(|b| (i64::from(b) * 3) % 997)
        .collect::<Vec<_>>();
    assert_eq!(index.encode("Hi", "en")?, expected);
    assert_eq!(
        length_to_mask(&[1, 3], Some(4)),
        (4, vec![1., 0., 0., 0., 1., 1., 1., 0.])
    );
    assert!(matches!(index.encode("𠮷", "en"), Err(Error::Model(_))));
    assert_eq!(
        get_latent_mask(&[0, 12, 13], 3, 4)?,
        (2, vec![0., 0., 1., 0., 1., 1.])
    );
    Ok(())
}

struct MarkSentence;
impl SentenceSynthesizer for MarkSentence {
    fn sentence(&mut self, text: &str, _voice: &str, _: f32) -> Result<Vec<f32>> {
        match text {
            "First." => Ok(vec![1., -1.]),
            "Second." => Ok(vec![0.5]),
            _ => Err(Error::Model(format!("wrong sentence: {text}"))),
        }
    }
}

#[test]
fn sentence_engine_emits_a_quarter_second_gap_and_observes_cancellation() -> Result<()> {
    let mut engine = SentenceEngine::new(MarkSentence, 24_000);
    let mut chunks = Vec::new();
    let stop = CancellationToken::new();
    engine.stream("First. Second.", "test", 1., &stop, &mut |chunk| {
        chunks.push(chunk.to_vec());
        true
    })?;
    assert_eq!(chunks[0], [1., -1.]);
    assert_eq!(chunks[1].len(), 6001);
    assert!(chunks[1][..6000].iter().all(|&s| s == 0.));
    assert_eq!(chunks[1][6000], 0.5);
    chunks.clear();
    engine.stream(
        "First. Must not be evaluated.",
        "test",
        1.,
        &stop,
        &mut |chunk| {
            chunks.push(chunk.to_vec());
            stop.cancel();
            true
        },
    )?;
    assert_eq!(chunks, [vec![1., -1.]]);
    Ok(())
}

#[test]
fn pcm_clamps_then_truncates_and_listen_decodes_signed_values() -> Result<()> {
    let values = [-2., -1., -0.5, 0., 0.5, 1., 2., f32::NAN];
    let int = [-32767i16, -32767, -16383, 0, 16383, 32767, 32767, 0];
    assert_eq!(pcm16(&values), int.map(i16::to_le_bytes).concat());
    assert_eq!(pcm16_to_float(&[0, 128, 0, 0, 0, 64])?, [-1., 0., 0.5]);
    assert!(pcm16_to_float(&[0]).is_err());
    Ok(())
}

enum VadEvent {
    Start(usize),
    End {
        when: usize,
        start: usize,
        end: usize,
    },
}
impl VadEvent {
    fn when(&self) -> usize {
        match *self {
            Self::Start(when) | Self::End { when, .. } => when,
        }
    }
}

struct ScriptVad {
    frame: usize,
    speaking: bool,
    script: VecDeque<VadEvent>,
}
impl VoiceActivityDetector for ScriptVad {
    fn accept(&mut self, _: &[f32; WINDOW]) -> Result<VadOutput> {
        self.frame += 1;
        let mut segment = None;
        if self
            .script
            .front()
            .is_some_and(|event| event.when() == self.frame)
        {
            match self.script.pop_front() {
                Some(VadEvent::Start(_)) => self.speaking = true,
                Some(VadEvent::End { start, end, .. }) => {
                    self.speaking = false;
                    segment = Some(SpeechSegment {
                        start,
                        samples: samples(start, end),
                    });
                }
                None => {}
            }
        }
        Ok(VadOutput {
            speaking: self.speaking,
            segment,
        })
    }
    fn reset(&mut self) {
        self.frame = 0;
        self.speaking = false;
    }
}
fn samples(begin: usize, end: usize) -> Vec<f32> {
    (begin..end).map(|n| n as f32).collect()
}
fn run(
    script: impl IntoIterator<Item = VadEvent>,
    preroll: f64,
    len: usize,
) -> Result<Vec<SegmenterEvent>> {
    let vad = ScriptVad {
        frame: 0,
        speaking: false,
        script: script.into_iter().collect(),
    };
    let mut segmenter = Segmenter::new(Box::new(vad), SAMPLE_RATE, preroll);
    let mut events = Vec::new();
    for chunk in samples(0, len).chunks(1600) {
        events.extend(segmenter.accept(chunk)?);
    }
    Ok(events)
}

#[test]
fn pre_roll_adds_quiet_prefix_as_a_contiguous_slice_across_input_frames() -> Result<()> {
    let detect = 54;
    let start = 25_000;
    let end = 32_000;
    let script = || {
        [
            VadEvent::Start(detect),
            VadEvent::End {
                when: 96,
                start,
                end,
            },
        ]
    };
    let without = run(script(), 0., 100 * WINDOW)?;
    let extended = run(script(), 750., 100 * WINDOW)?;
    assert_eq!(
        without,
        [
            SegmenterEvent::SpeechStart,
            SegmenterEvent::SpeechEnd(samples(start, end))
        ]
    );
    assert_eq!(
        extended,
        [
            SegmenterEvent::SpeechStart,
            SegmenterEvent::SpeechEnd(samples(detect * WINDOW - 12_000, end))
        ]
    );
    Ok(())
}

#[test]
fn pre_roll_never_crosses_previous_audio_and_long_continuations_are_not_prefixed() -> Result<()> {
    let events = run(
        [
            VadEvent::Start(30),
            VadEvent::End {
                when: 50,
                start: 13_000,
                end: 18_000,
            },
            VadEvent::Start(51),
            VadEvent::End {
                when: 75,
                start: 22_000,
                end: 32_000,
            },
            VadEvent::Start(76),
            VadEvent::End {
                when: 190,
                start: 51_000,
                end: 85_000,
            },
        ],
        750.,
        192 * WINDOW,
    )?;
    assert_eq!(
        events,
        [
            SegmenterEvent::SpeechStart,
            SegmenterEvent::SpeechEnd(samples(3360, 18_000)),
            SegmenterEvent::SpeechStart,
            SegmenterEvent::SpeechEnd(samples(18_000, 32_000)),
            SegmenterEvent::SpeechStart,
            SegmenterEvent::SpeechEnd(samples(51_000, 85_000)),
        ]
    );
    Ok(())
}

#[test]
fn pocket_lists_only_packaged_prompt_states_in_catalog_order() -> Result<()> {
    use pi_rs_voice_server::models::pocket_voices;
    use std::{fs, path::PathBuf};

    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let path = PathBuf::from("target").join(format!("pocket-voice-catalog-{}", std::process::id()));
    fs::create_dir_all(&path)?;
    let fixture = Fixture(path);
    for file in [
        "vera.safetensors",
        "alba.safetensors",
        "peter_yearsley.safetensors",
        "invented.safetensors",
        "anna.json",
    ] {
        fs::write(fixture.0.join(file), [])?;
    }
    let voices = pocket_voices(&fixture.0)?;
    let names = voices
        .iter()
        .map(|v| (v.id.as_str(), v.name.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            ("alba", "Alba"),
            ("peter_yearsley", "Peter Yearsley"),
            ("vera", "Vera")
        ]
    );
    Ok(())
}
