//! Text access and editing hooks for custom TUI editors.

use crate::{autocomplete::AutocompleteProvider, tui::Component};

pub type EditorCallback = Box<dyn FnMut(&str)>;
pub type EditorBorder = Box<dyn Fn(&str) -> String>;

pub trait EditorComponent: Component {
    fn text(&self) -> &str;
    fn set_text(&mut self, text: &str);
    fn set_on_submit(&mut self, callback: Option<EditorCallback>);
    fn set_on_change(&mut self, callback: Option<EditorCallback>);

    fn expanded_text(&self) -> std::borrow::Cow<'_, str> {
        self.text().into()
    }
}

pub trait EditorHistory: EditorComponent {
    fn add_to_history(&mut self, text: &str);
}

pub trait EditorInsertion: EditorComponent {
    fn insert_text_at_cursor(&mut self, text: &str);
}

pub trait EditorAutocomplete: EditorComponent {
    fn set_autocomplete_provider(&mut self, provider: Box<dyn AutocompleteProvider>);
    fn set_autocomplete_max_visible(&mut self, max_visible: usize);
}

pub trait EditorAppearance: EditorComponent {
    fn set_border_color(&mut self, color: EditorBorder);
    fn set_padding_x(&mut self, padding: usize);
}
