//! The command menu's highlighted option, as pure state (upstream `command-menu/highlight.ts`).
//! The DOM version keeps `aria-selected` on one option; the port keeps an index into the
//! option list so the behavior is testable without a browser.

use super::context::ENTER_WAIT_MS;

/// An option in the listbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListOption {
    pub id: String,
    /// `data-preferred`: chosen as the initial highlight while the input is empty.
    pub preferred: bool,
}

/// The highlight state of one listbox.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Highlight {
    pub options: Vec<ListOption>,
    /// Index of the highlighted option, if any.
    pub selected: Option<usize>,
    /// `data-moved`: the reader moved the highlight (a key or pointer), not the initial choice.
    pub moved: bool,
    /// An Enter that found no option, with the query it was pressed on and when (ms).
    pub waiting: Option<(String, u64)>,
}

impl Highlight {
    pub fn new(options: Vec<ListOption>) -> Self {
        let mut state = Self {
            options,
            ..Self::default()
        };
        state.highlight_first();
        state
    }

    /// The option highlighted until the reader moves: the preferred one while nothing is
    /// typed (given by `query`), else the first.
    fn initial(&self, query: &str) -> Option<usize> {
        if query.is_empty()
            && let Some(index) = self.options.iter().position(|option| option.preferred)
        {
            return Some(index);
        }
        if self.options.is_empty() {
            None
        } else {
            Some(0)
        }
    }

    /// Highlights the first option (the preferred one while the input is empty), or none:
    /// the query or page changed, the reader starts over.
    pub fn highlight_first(&mut self) {
        self.moved = false;
        self.waiting = None;
        let initial = self.initial("");
        self.selected = initial;
    }

    /// Re-highlights for a new query, clearing a pending Enter.
    pub fn set_query(&mut self, query: &str) {
        self.waiting = None;
        self.selected = self.initial(query);
    }

    /// Replaces the options (rows came or went), keeping the highlight per upstream rules.
    pub fn set_options(
        &mut self,
        options: Vec<ListOption>,
        query: &str,
        now: u64,
    ) -> Option<usize> {
        self.options = options;
        if !self.moved {
            self.selected = self.initial(query);
        } else if self.selected.is_none()
            || self
                .selected
                .is_some_and(|index| index >= self.options.len())
        {
            self.highlight_first();
        }
        // A pending Enter acts on the option that rendered soon, if the query still matches.
        let pending = self.waiting.take();
        if let (Some((text, at)), Some(index)) = (pending, self.selected)
            && text == query
            && now.saturating_sub(at) < ENTER_WAIT_MS
        {
            return Some(index);
        }
        None
    }

    /// An Enter on `query`: the highlighted option is clicked; while there is none, its
    /// text and time are remembered so the first option that renders soon is clicked instead.
    pub fn enter(&mut self, query: &str, now: u64) -> Option<usize> {
        match self.selected {
            Some(index) => Some(index),
            None => {
                self.waiting = Some((query.to_owned(), now));
                None
            }
        }
    }

    /// Highlights `option` (a pointer moved over it), in the listbox it belongs to.
    pub fn highlight(&mut self, id: &str) {
        if let Some(index) = self.options.iter().position(|option| option.id == id) {
            self.moved = true;
            self.selected = Some(index);
        }
    }

    /// Moves the highlight `step` options down (up when negative), wrapping.
    pub fn move_highlight(&mut self, step: i32) {
        if self.options.is_empty() {
            return;
        }
        let len = self.options.len() as i32;
        let current = match self.selected {
            Some(index) => index as i32,
            None if step > 0 => -1,
            None => 0,
        };
        let next = (current + step).rem_euclid(len) as usize;
        self.moved = true;
        self.selected = Some(next);
    }

    /// The id of the highlighted option.
    pub fn highlighted_id(&self) -> Option<&str> {
        self.selected
            .and_then(|index| self.options.get(index))
            .map(|option| option.id.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(names: &[&str]) -> Vec<ListOption> {
        names
            .iter()
            .map(|name| ListOption {
                id: (*name).into(),
                preferred: false,
            })
            .collect()
    }

    #[test]
    fn initial_is_first_or_preferred() {
        let highlight = Highlight::new(opts(&["a", "b"]));
        assert_eq!(highlight.highlighted_id(), Some("a"));
        let preferred = vec![
            ListOption {
                id: "a".into(),
                preferred: false,
            },
            ListOption {
                id: "b".into(),
                preferred: true,
            },
        ];
        let highlight = Highlight::new(preferred);
        assert_eq!(highlight.highlighted_id(), Some("b"));
    }

    #[test]
    fn arrow_moves_wrap_and_mark_moved() {
        let mut highlight = Highlight::new(opts(&["a", "b"]));
        highlight.move_highlight(1);
        assert_eq!(highlight.highlighted_id(), Some("b"));
        highlight.move_highlight(1);
        assert_eq!(highlight.highlighted_id(), Some("a"));
        highlight.move_highlight(-1);
        assert_eq!(highlight.highlighted_id(), Some("b"));
        assert!(highlight.moved);
    }

    #[test]
    fn pointer_highlight_marks_moved() {
        let mut highlight = Highlight::new(opts(&["a", "b"]));
        highlight.highlight("b");
        assert_eq!(highlight.highlighted_id(), Some("b"));
        assert!(highlight.moved);
    }

    #[test]
    fn new_rows_keep_initial_until_moved() {
        let mut highlight = Highlight::new(opts(&["a"]));
        assert_eq!(highlight.set_options(opts(&["new", "a"]), "", 0), None);
        assert_eq!(highlight.highlighted_id(), Some("new"));
        highlight.move_highlight(1);
        assert_eq!(highlight.highlighted_id(), Some("a"));
    }

    #[test]
    fn pending_enter_waits_for_first_rendered_option() {
        let mut highlight = Highlight::new(vec![]);
        assert_eq!(highlight.enter("new", 100), None);
        assert_eq!(highlight.set_options(opts(&["row"]), "new", 200), Some(0));
    }

    #[test]
    fn pending_enter_expires_and_requires_same_query() {
        let mut highlight = Highlight::new(vec![]);
        assert_eq!(highlight.enter("q", 0), None);
        assert_eq!(
            highlight.set_options(opts(&["row"]), "q", ENTER_WAIT_MS + 1),
            None
        );
        let mut highlight = Highlight::new(vec![]);
        assert_eq!(highlight.enter("q", 100), None);
        assert_eq!(highlight.set_options(opts(&["row"]), "other", 200), None);
    }
}
