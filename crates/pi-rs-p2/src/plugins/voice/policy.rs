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
pub fn barge_in_phase(hearing: bool, playing: bool) -> BargeInPhase {
    if hearing {
        BargeInPhase::Hearing
    } else if playing {
        BargeInPhase::Speaking
    } else {
        BargeInPhase::Listening
    }
}
