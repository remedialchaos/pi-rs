use crate::render_context::{RenderContext, line_text, more_lines_hint, safe_text};
use pi_rs_tui::{Component, Container, Loader, Text};

const PREVIEW_LINES: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BashStatus {
    Running,
    Complete,
    Cancelled,
    Error,
}

pub struct BashExecutionComponent {
    command: String,
    output_lines: Vec<String>,
    status: BashStatus,
    exit_code: Option<i32>,
    error: Option<String>,
    loader: Option<Loader>,
    truncated: bool,
    full_output_path: Option<String>,
    expanded: bool,
    content_container: Container,
    color_key: ColorKey,
    output_pad: usize,
    ctx: Box<dyn RenderContext>,
    by: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorKey {
    Dim,
    BashMode,
}

impl BashExecutionComponent {
    pub fn new(
        command: String,
        ctx: Box<dyn RenderContext>,
        options: BashExecutionOptions,
    ) -> Self {
        let color_key = if options.exclude_from_context {
            ColorKey::Dim
        } else {
            ColorKey::BashMode
        };
        let mut content_container = Container::default();
        content_container.add_child(pi_rs_tui::ComponentRef::new(Text::new(
            safe_text(&command),
            0,
            0,
            None,
        )));

        let loader = options.request_render.map(|request_render| {
            Loader::new(
                request_render,
                std::rc::Rc::new(str::to_owned),
                std::rc::Rc::new(str::to_owned),
                "Running".into(),
                None,
            )
        });

        let mut component = Self {
            command,
            output_lines: Vec::new(),
            status: BashStatus::Running,
            exit_code: None,
            error: None,
            loader,
            truncated: false,
            full_output_path: None,
            expanded: false,
            content_container,
            color_key,
            output_pad: options.output_pad.unwrap_or(1),
            ctx,
            by: options.by,
        };
        component.update_display();
        component
    }

    pub fn set_expanded(&mut self, expanded: bool) {
        if expanded == self.expanded {
            return;
        }
        self.expanded = expanded;
        self.update_display();
    }

    pub fn set_output(&mut self, output: String, truncated: bool) {
        let mut tail = output.as_str();
        if let Some((index, _)) = output.rmatch_indices('\n').nth(1999) {
            tail = &output[index + 1..];
        }
        if tail.len() > 50 * 1024 {
            let cutoff = tail.ceil_char_boundary(tail.len() - 50 * 1024);
            tail = if tail.as_bytes().get(cutoff.saturating_sub(1)) == Some(&b'\n') {
                &tail[cutoff..]
            } else if let Some(index) = tail[cutoff..].find('\n') {
                &tail[cutoff + index + 1..]
            } else {
                &tail[cutoff..]
            };
        }
        self.truncated = truncated || tail.len() < output.len();
        self.output_lines = safe_text(tail).split('\n').map(str::to_owned).collect();
        self.update_display();
    }

    pub fn set_complete(
        &mut self,
        exit_code: Option<i32>,
        cancelled: bool,
        options: BashCompleteOptions,
    ) {
        self.status = if cancelled {
            BashStatus::Cancelled
        } else if options.error.is_some() || exit_code.is_some_and(|code| code != 0) {
            BashStatus::Error
        } else {
            BashStatus::Complete
        };
        self.exit_code = exit_code;
        self.truncated |= options.truncated.unwrap_or(false);
        self.full_output_path = options.full_output_path;
        self.error = options.error;
        if let Some(l) = self.loader.as_mut() {
            l.stop()
        }
        self.update_display();
    }

    pub fn dispose(&mut self) {
        if let Some(l) = self.loader.as_mut() {
            l.stop()
        }
    }

    fn update_display(&mut self) {
        self.content_container.clear();
        let theme = self.ctx.theme().current();

        let token = match self.color_key {
            ColorKey::Dim => "dim",
            ColorKey::BashMode => "bashMode",
        };
        let mut header = theme.fg(
            token,
            &theme.bold(&format!("$ {}", line_text(&self.command))),
        );
        if let Some(by) = &self.by {
            header.push_str(&theme.fg("muted", &format!(" · {}", line_text(by))));
        }
        self.content_container
            .add_child(pi_rs_tui::ComponentRef::new(Text::new(
                header,
                self.output_pad,
                0,
                None,
            )));

        if self.expanded || self.output_lines.len() <= PREVIEW_LINES {
            for line in &self.output_lines {
                self.content_container
                    .add_child(pi_rs_tui::ComponentRef::new(Text::new(
                        line_text(line),
                        0,
                        0,
                        None,
                    )));
            }
        } else {
            for line in self.output_lines.iter().rev().take(PREVIEW_LINES).rev() {
                self.content_container
                    .add_child(pi_rs_tui::ComponentRef::new(Text::new(
                        line_text(line),
                        0,
                        0,
                        None,
                    )));
            }
            self.content_container
                .add_child(pi_rs_tui::ComponentRef::new(Text::new(
                    more_lines_hint(
                        self.ctx.as_ref(),
                        &theme,
                        self.output_lines.len() - PREVIEW_LINES,
                        "lines",
                    ),
                    0,
                    0,
                    None,
                )));
        }

        let mut status = Vec::new();
        match self.status {
            BashStatus::Running => {}
            BashStatus::Cancelled => status.push(theme.fg("warning", "(cancelled)")),
            BashStatus::Error => {
                let reason = self
                    .error
                    .as_ref()
                    .map(|s| line_text(s))
                    .or_else(|| self.exit_code.map(|n| format!("exit {n}")))
                    .unwrap_or_default();
                status.push(theme.fg("error", &format!("({reason})")));
            }
            BashStatus::Complete => {}
        }
        if self.truncated
            && let Some(path) = &self.full_output_path
        {
            status.push(theme.fg(
                "warning",
                &format!("Output truncated. Full output: {}", line_text(path)),
            ));
        }
        if !status.is_empty() {
            self.content_container
                .add_child(pi_rs_tui::ComponentRef::new(Text::new(
                    format!("\n{}", status.join("\n")),
                    self.output_pad,
                    0,
                    None,
                )));
        }
    }
}

impl Component for BashExecutionComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = self.content_container.render(width);
        if self.status == BashStatus::Running
            && let Some(loader) = &self.loader
        {
            lines.extend(loader.render(width));
        }
        lines
    }

    fn invalidate(&mut self) {
        self.content_container.invalidate();
    }
}

#[derive(Clone, Default)]
pub struct BashExecutionOptions {
    pub exclude_from_context: bool,
    pub output_pad: Option<usize>,
    pub request_render: Option<std::rc::Rc<dyn Fn()>>,
    pub by: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct BashCompleteOptions {
    pub truncated: Option<bool>,
    pub full_output_path: Option<String>,
    pub error: Option<String>,
}

pub fn shell_author(author: ShellAuthor) -> Option<String> {
    match author {
        ShellAuthor::Guest { name } => Some(format!("{} (guest)", name)),
        ShellAuthor::Owner => None,
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ShellAuthor {
    Owner,
    Guest { name: String },
}

pub type TuiEntryRenderer = Box<
    dyn Fn(
        &crate::components::transcript::EntryRecord,
        Box<dyn RenderContext>,
        bool,
    ) -> Option<Box<dyn Component>>,
>;
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShellData {
    author: ShellAuthor,
    command: String,
    output: String,
    exit_code: Option<i32>,
    cancelled: bool,
    truncated: bool,
    full_output_path: Option<String>,
    include_in_context: bool,
    error: Option<String>,
}
pub fn shell_entry_renderer(
    entry: &crate::components::transcript::EntryRecord,
    ctx: Box<dyn RenderContext>,
    expanded: bool,
) -> Option<Box<dyn Component>> {
    let data: ShellData = serde_json::from_value(entry.data.clone()).ok()?;
    let mut component = BashExecutionComponent::new(
        data.command,
        ctx,
        BashExecutionOptions {
            exclude_from_context: !data.include_in_context,
            by: shell_author(data.author),
            ..Default::default()
        },
    );
    component.set_expanded(expanded);
    component.set_output(data.output, data.truncated);
    component.set_complete(
        data.exit_code,
        data.cancelled,
        BashCompleteOptions {
            truncated: Some(data.truncated),
            full_output_path: data.full_output_path,
            error: data.error,
        },
    );
    Some(Box::new(component))
}
