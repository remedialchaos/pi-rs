//! Barge-in detector phase policy.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BargeInPhase {
    Inactive,
    Listening,
    Hearing,
    Thinking,
    Tool,
    Speaking,
    Error,
}

/// Maps `{ hearing, playing }` to the barge-in detector phase.
use regex::Regex;
use std::sync::LazyLock;

pub fn barge_in_phase(hearing: bool, playing: bool) -> BargeInPhase {
    if hearing {
        BargeInPhase::Hearing
    } else if playing {
        BargeInPhase::Speaking
    } else {
        BargeInPhase::Listening
    }
}

/// An audio output device as `enumerateDevices` lists it (label readable once mic is allowed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioOutput {
    pub kind: String,
    pub device_id: String,
    pub label: String,
}

static WORN: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"(?i)head ?phones?|ear ?phones?|headset|ear ?buds?|airpods|in-ear|kopfh[oö]rer")
        .ok()
});

/// Whether the microphone should enable browser echo cancellation for the listed outputs.
/// Off for worn outputs (headphones/earbuds/headsets) so Chrome's canceller does not suppress
/// the user's own voice; on for speakers or unknown outputs.
pub fn echo_cancellation_for(devices: &[AudioOutput]) -> bool {
    let outputs: Vec<&AudioOutput> = devices.iter().filter(|d| d.kind == "audiooutput").collect();
    let used = outputs
        .iter()
        .copied()
        .find(|d| d.device_id == "default")
        .or_else(|| outputs.first().copied());
    match (used, WORN.as_ref()) {
        (Some(device), Some(pattern)) => !pattern.is_match(&device.label),
        _ => true,
    }
}
