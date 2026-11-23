use crate::components::selectors::SelectorContext;
use crate::render_context::line_text;
use pi_rs_tui::{Component, Focusable};

#[derive(Debug, Clone)]
pub struct UserMessageItem {
    pub id: u32,
    pub text: String,
}

struct UserMessageList {
    ctx: SelectorContext,
    messages: Vec<UserMessageItem>,
    selected_index: usize,
    on_select: Option<Box<dyn Fn(u32) + Send + Sync>>,
    on_cancel: Option<Box<dyn Fn() + Send + Sync>>,
    max_visible: usize,
}

impl UserMessageList {
    pub fn new(
        ctx: SelectorContext,
        messages: Vec<UserMessageItem>,
        initial_selected_id: Option<u32>,
    ) -> Self {
        let selected_index = initial_selected_id
            .and_then(|id| messages.iter().position(|m| m.id == id))
            .unwrap_or(0);
        Self {
            ctx,
            messages,
            selected_index,
            on_select: None,
            on_cancel: None,
            max_visible: 10,
        }
    }
}

impl Component for UserMessageList {
    fn render(&self, _width: usize) -> Vec<String> {
        let theme = self.ctx.theme.theme().current();
        let mut lines = Vec::new();

        let start = self.selected_index.saturating_sub(self.max_visible / 2);
        let end = (start + self.max_visible).min(self.messages.len());

        for (i, msg) in self
            .messages
            .iter()
            .enumerate()
            .skip(start)
            .take(end - start)
        {
            let prefix = if i == self.selected_index {
                "→ "
            } else {
                "  "
            };
            let text = line_text(&msg.text);
            lines.push(format!("{}{}", prefix, theme.fg("text", &text)));
        }

        lines
    }

    fn invalidate(&mut self) {}
}

impl UserMessageList {
    fn handle_input(&mut self, data: &str) {
        let keys = &self.ctx.keys;
        if keys.matches(data, "tui.select.up") {
            if self.selected_index > 0 {
                self.selected_index -= 1;
            }
        } else if keys.matches(data, "tui.select.down") {
            if self.selected_index < self.messages.len().saturating_sub(1) {
                self.selected_index += 1;
            }
        } else if keys.matches(data, "tui.select.confirm") {
            if let (Some(on_select), Some(row)) =
                (&self.on_select, self.messages.get(self.selected_index))
            {
                (on_select)(row.id);
            }
        } else if keys.matches(data, "tui.select.cancel")
            && let Some(on_cancel) = &self.on_cancel
        {
            (on_cancel)();
        }
    }
}

pub struct UserMessageSelectorComponent {
    message_list: UserMessageList,
    focused: bool,
}

impl UserMessageSelectorComponent {
    pub fn new(
        ctx: SelectorContext,
        messages: Vec<UserMessageItem>,
        on_select: impl Fn(u32) + Send + Sync + 'static,
        on_cancel: impl Fn() + Send + Sync + 'static,
        initial_selected_id: Option<u32>,
    ) -> Self {
        let mut message_list = UserMessageList::new(ctx, messages, initial_selected_id);
        message_list.on_select = Some(Box::new(on_select));
        message_list.on_cancel = Some(Box::new(on_cancel));
        Self {
            message_list,
            focused: false,
        }
    }
}

impl Component for UserMessageSelectorComponent {
    fn render(&self, width: usize) -> Vec<String> {
        self.message_list.render(width)
    }

    fn handle_input(&mut self, data: &str) {
        self.message_list.handle_input(data);
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }

    fn invalidate(&mut self) {}
}

impl Focusable for UserMessageSelectorComponent {
    fn is_focused(&self) -> bool {
        self.focused
    }
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}
