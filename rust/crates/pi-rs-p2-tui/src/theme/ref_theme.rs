use super::Theme;
use pi_rs_tui::components::TextStyleFn;
use pi_rs_tui::{EditorTheme, MarkdownTheme, SelectListTheme, SettingsListTheme};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

type Listener = Rc<dyn Fn(&Theme)>;

#[derive(Clone)]
pub struct ThemeRef {
    theme: Rc<RefCell<Theme>>,
    version: Rc<Cell<u64>>,
    listeners: Rc<RefCell<Vec<Listener>>>,
}

impl ThemeRef {
    pub fn new(theme: Theme) -> Self {
        Self {
            theme: Rc::new(RefCell::new(theme)),
            version: Rc::default(),
            listeners: Rc::default(),
        }
    }
    pub fn current(&self) -> Theme {
        self.theme.borrow().clone()
    }
    pub fn version(&self) -> u64 {
        self.version.get()
    }
    pub fn set(&self, theme: Theme) {
        self.theme.replace(theme.clone());
        self.version.set(self.version.get() + 1);
        let listeners = self.listeners.borrow().clone();
        for listener in listeners {
            listener(&theme);
        }
    }
    pub fn subscribe<F: Fn(&Theme) + 'static>(&self, listener: F) -> impl FnOnce() + use<F> {
        let listener: Listener = Rc::new(listener);
        let listeners = self.listeners.clone();
        listeners.borrow_mut().push(listener.clone());
        move || {
            listeners
                .borrow_mut()
                .retain(|item| !Rc::ptr_eq(item, &listener))
        }
    }
    pub fn fg(&self, token: &'static str) -> TextStyleFn {
        let theme = self.theme.clone();
        Rc::new(move |text| theme.borrow().fg(token, text))
    }
    pub fn markdown(&self) -> MarkdownTheme {
        let theme = self.theme.clone();
        MarkdownTheme {
            heading: self.fg("mdHeading"),
            link: self.fg("mdLink"),
            link_url: self.fg("mdLinkUrl"),
            code: self.fg("mdCode"),
            code_block: self.fg("mdCodeBlock"),
            code_block_border: self.fg("mdCodeBlockBorder"),
            quote: self.fg("mdQuote"),
            quote_border: self.fg("mdQuoteBorder"),
            hr: self.fg("mdHr"),
            list_bullet: self.fg("mdListBullet"),
            highlight_code: Some(Rc::new(move |code, lang| {
                theme
                    .borrow()
                    .highlight(code, lang.unwrap_or_default())
                    .split('\n')
                    .map(str::to_owned)
                    .collect()
            })),
            ..Default::default()
        }
    }
    pub fn select_list(&self) -> SelectListTheme {
        SelectListTheme {
            selected_text: self.fg("accent"),
            description: self.fg("muted"),
            scroll_info: self.fg("muted"),
            no_match: self.fg("muted"),
        }
    }
    pub fn editor(&self) -> EditorTheme {
        EditorTheme {
            border_color: self.fg("borderMuted"),
            select_list: self.select_list(),
        }
    }
    pub fn settings_list(&self) -> SettingsListTheme {
        let label = self.fg("accent");
        let selected_value = self.fg("accent");
        let value = self.fg("muted");
        SettingsListTheme {
            label: Rc::new(move |text, selected| {
                if selected {
                    label(text)
                } else {
                    text.to_owned()
                }
            }),
            value: Rc::new(move |text, selected| {
                if selected {
                    selected_value(text)
                } else {
                    value(text)
                }
            }),
            description: self.fg("dim"),
            cursor: self.theme.borrow().fg("accent", "→ "),
            hint: self.fg("dim"),
        }
    }
}
