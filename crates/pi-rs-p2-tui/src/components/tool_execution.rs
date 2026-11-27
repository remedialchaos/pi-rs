use crate::render_context::RenderContext;
use crate::tools::render_utils::{format_tool_call_with_args, get_text_output};
use crate::tools::types::{ToolRenderContext, ToolRenderers, ToolResultView, ToolView};
use pi_rs_tui::{Component, Container, Text};
use std::collections::HashMap;

type KeyText = std::rc::Rc<dyn Fn(&str) -> String + Send + Sync>;
#[derive(Clone)]
pub struct ToolCardContext {
    pub cwd: String,
    pub output_pad: Option<usize>,
    pub theme: crate::theme::ThemeRef,
    pub key_text: Option<KeyText>,
    pub request_render: Option<std::rc::Rc<dyn Fn() + Send + Sync>>,
}

impl RenderContext for ToolCardContext {
    fn theme(&self) -> &crate::theme::ThemeRef {
        &self.theme
    }
    fn key_text(&self, id: &str) -> String {
        self.key_text.as_ref().map(|f| f(id)).unwrap_or_default()
    }
    fn request_render(&self) {
        if let Some(f) = self.request_render.as_ref() {
            f()
        }
    }
}

pub struct ToolExecutionComponent {
    call_renderer_component: Option<Box<dyn Component>>,
    result_renderer_component: Option<Box<dyn Component>>,
    renderer_state: HashMap<String, serde_json::Value>,
    tool: ToolView,
    args_complete: bool,
    expanded: bool,
    renderers: ToolRenderers,
    ctx: ToolCardContext,
    started_at_ms: u64,
    ended_at_ms: Option<u64>,
}

impl ToolExecutionComponent {
    pub fn new(
        tool: ToolView,
        renderers: Option<ToolRenderers>,
        ctx: ToolCardContext,
        args_complete: bool,
    ) -> Self {
        let renderers = renderers.unwrap_or_default();
        let started_at_ms = crate::tools::shell::now_epoch_ms();
        let ended_at_ms = (!matches!(
            tool.status,
            crate::tools::ToolStatus::Pending | crate::tools::ToolStatus::Running
        ))
        .then_some(started_at_ms);
        let mut renderer_state = HashMap::new();
        renderer_state.insert("startedAt".into(), serde_json::json!(started_at_ms));
        if let Some(ended) = ended_at_ms {
            renderer_state.insert("endedAt".into(), serde_json::json!(ended));
        }
        let mut card = Self {
            call_renderer_component: None,
            result_renderer_component: None,
            renderer_state,
            tool,
            args_complete,
            expanded: false,
            renderers,
            ctx,
            started_at_ms,
            ended_at_ms,
        };
        card.update_display();
        card
    }

    pub fn tool_id(&self) -> &str {
        &self.tool.id
    }

    pub fn elapsed_ms(&self) -> Option<u64> {
        self.ended_at_ms
            .map(|ended| ended.saturating_sub(self.started_at_ms))
    }

    pub fn update(&mut self, tool: ToolView, args_complete: bool) {
        self.tool = tool;
        self.args_complete = args_complete;
        if !self.is_partial() && self.ended_at_ms.is_none() {
            let ended = crate::tools::shell::now_epoch_ms();
            self.ended_at_ms = Some(ended);
            self.renderer_state
                .insert("endedAt".into(), serde_json::json!(ended));
        }
        self.update_display();
    }

    pub fn set_expanded(&mut self, expanded: bool) {
        if expanded == self.expanded {
            return;
        }
        self.expanded = expanded;
        self.update_display();
    }

    pub fn dispose(&mut self) {
        // Stop any timers
    }

    fn calls(&self) -> Option<Vec<crate::tools::types::NestedCall>> {
        self.renderers
            .nested_calls
            .as_ref()
            .and_then(|f| f(&self.tool))
            .or_else(|| self.tool.calls.clone())
    }

    fn is_partial(&self) -> bool {
        matches!(
            self.tool.status,
            crate::tools::ToolStatus::Pending | crate::tools::ToolStatus::Running
        )
    }

    fn is_error(&self) -> bool {
        matches!(
            self.tool.status,
            crate::tools::ToolStatus::Error | crate::tools::ToolStatus::Interrupted
        )
    }

    fn result(&self) -> Option<ToolResultView> {
        self.tool.result.clone()
    }

    fn render_context(&self, last_component: Option<Box<dyn Component>>) -> ToolRenderContext {
        ToolRenderContext {
            args: self.tool.args.clone(),
            tool_call_id: self.tool.id.clone(),
            invalidate: Box::new(|| {}),
            last_component,
            state: self.renderer_state.clone(),
            cwd: self.ctx.cwd.clone(),
            execution_started: true,
            args_complete: self.args_complete,
            is_partial: self.is_partial(),
            expanded: self.expanded,
            show_images: false,
            is_error: self.is_error(),
            duration_ms: self
                .ended_at_ms
                .map(|ended| ended.saturating_sub(self.started_at_ms)),
            output_pad: self.ctx.output_pad.unwrap_or(1),
            calls: self.calls(),
            ui: Box::new(self.ctx.clone()),
        }
    }

    fn create_call_fallback(&self) -> Box<dyn Component> {
        let theme = &self.ctx.theme.current();
        let mut container = Container::default();
        let text = format_tool_call_with_args(
            &self.tool.name,
            &serde_json::json!(self.tool.args),
            theme,
            self.expanded,
        );
        container.add_child(pi_rs_tui::ComponentRef::new(Text::new(text, 0, 0, None)));
        Box::new(container)
    }

    fn create_result_fallback(&self, result: &ToolResultView) -> Option<Box<dyn Component>> {
        let theme = &self.ctx.theme.current();
        let output = get_text_output(Some(result));
        if output.trim().is_empty() {
            return None;
        }
        let mut container = Container::default();
        container.add_child(pi_rs_tui::ComponentRef::new(Text::new(
            theme.fg("toolOutput", &output),
            0,
            0,
            None,
        )));
        Some(Box::new(container))
    }

    fn update_display(&mut self) {
        let theme = &self.ctx.theme.current();

        // Render call
        if let Some(render_call) = &self.renderers.render_call {
            let previous = self.call_renderer_component.take();
            let ctx = self.render_context(previous);
            self.call_renderer_component = Some(render_call(&self.tool.args, theme, &ctx));
        } else {
            self.call_renderer_component = Some(self.create_call_fallback());
        }

        // Render result
        if self.result().is_none() {
            self.result_renderer_component = None;
        }
        if let Some(result) = self.result() {
            if let Some(render_result) = &self.renderers.render_result {
                let options = crate::tools::types::ToolRenderResultOptions {
                    expanded: self.expanded,
                    is_partial: self.is_partial(),
                };
                let previous = self.result_renderer_component.take();
                let ctx = self.render_context(previous);
                self.result_renderer_component =
                    Some(render_result(&result, &options, theme, &ctx));
            } else {
                self.result_renderer_component = self.create_result_fallback(&result);
            }
        }
    }
}

impl Component for ToolExecutionComponent {
    fn render(&self, width: usize) -> Vec<String> {
        self.call_renderer_component
            .iter()
            .chain(self.result_renderer_component.iter())
            .flat_map(|c| c.render(width))
            .collect()
    }

    fn invalidate(&mut self) {
        for c in self
            .call_renderer_component
            .iter_mut()
            .chain(self.result_renderer_component.iter_mut())
        {
            c.invalidate();
        }
    }
}
