//! Haptic feedback (upstream `haptics.ts`).

/// A haptic action the page asks the iOS bridge for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Haptic {
    /// A light tap: a message was sent.
    LightTap,
    /// Start of a selection sequence (a wheel detent).
    SelectionStart,
    /// A selection tick.
    SelectionChanged,
}

impl Haptic {
    /// The Capacitor `Haptics` plugin method and options for this action.
    pub fn call(self) -> (&'static str, &'static str) {
        match self {
            Self::LightTap => ("impact", "LIGHT"),
            Self::SelectionStart => ("selectionStart", ""),
            Self::SelectionChanged => ("selectionChanged", ""),
        }
    }

    /// The JS expression the host evaluates on `window.Capacitor.nativePromise`.
    pub fn native_promise(self) -> String {
        let (method, style) = self.call();
        if style.is_empty() {
            format!("nativePromise('Haptics', '{method}', {{}})")
        } else {
            format!("nativePromise('Haptics', '{method}', {{ style: '{style}' }})")
        }
    }
}

/// The next haptic for a tick, tracking whether the selection sequence has started.
#[derive(Clone, Copy, Debug, Default)]
pub struct TickSequence {
    started: bool,
}

impl TickSequence {
    /// Actions for a tick: the first opens the sequence, every tick then changes selection.
    pub fn tick(&mut self) -> Vec<Haptic> {
        let mut actions = vec![];
        if !self.started {
            self.started = true;
            actions.push(Haptic::SelectionStart);
        }
        actions.push(Haptic::SelectionChanged);
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn haptic_calls() {
        assert_eq!(Haptic::LightTap.call(), ("impact", "LIGHT"));
        assert_eq!(
            Haptic::LightTap.native_promise(),
            "nativePromise('Haptics', 'impact', { style: 'LIGHT' })"
        );
        assert_eq!(
            Haptic::SelectionChanged.native_promise(),
            "nativePromise('Haptics', 'selectionChanged', {})"
        );
    }

    #[test]
    fn tick_sequence_starts_once() {
        let mut sequence = TickSequence::default();
        assert_eq!(
            sequence.tick(),
            vec![Haptic::SelectionStart, Haptic::SelectionChanged]
        );
        assert_eq!(sequence.tick(), vec![Haptic::SelectionChanged]);
    }
}
