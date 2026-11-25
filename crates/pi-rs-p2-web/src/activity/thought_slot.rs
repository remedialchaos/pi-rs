//! The thought a working wheel shows below it (upstream `wheelThought` in `activity/model.ts`).

use super::model::{WheelEntry, Words};

/// The newest thought among a working wheel's entries (and none once the wheel is done).
pub fn wheel_thought(entries: &[WheelEntry], working: bool) -> Option<Words> {
    if !working {
        return None;
    }
    entries.iter().rev().find_map(|entry| match entry {
        WheelEntry::Words(words) if words.kind == super::model::WordsKind::Thought => {
            Some(words.clone())
        }
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::super::model::{Step, StepStatus, WordsKind};
    use super::*;

    fn words(id: &str, kind: WordsKind) -> WheelEntry {
        WheelEntry::Words(Words {
            kind,
            id: id.into(),
            text: format!("text {id}"),
            live: false,
        })
    }

    #[test]
    fn newest_thought_wins() {
        let entries = vec![
            words("a", WordsKind::Thought),
            WheelEntry::Call(Step::new("1", "bash", StepStatus::Done)),
            words("b", WordsKind::Thought),
            words("c", WordsKind::Text),
        ];
        let thought = wheel_thought(&entries, true).unwrap();
        assert_eq!(thought.id, "b");
    }

    #[test]
    fn done_wheel_shows_no_thought() {
        let entries = vec![words("a", WordsKind::Thought)];
        assert!(wheel_thought(&entries, false).is_none());
    }
}
