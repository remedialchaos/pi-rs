use crate::render_context::RenderContext;
use crate::theme::Theme;
use pi_rs_ai::TextContent;
use pi_rs_tui::Component;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

pub type ToolArgs = HashMap<String, Value>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultView {
    pub content: Vec<ToolResultContent>,
    pub details: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ToolResultContent {
    Text(TextContent),
    Image(ImageRef),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageRef {
    pub url: String,
    pub mime_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolRenderResultOptions {
    pub expanded: bool,
    pub is_partial: bool,
}

pub struct ToolRenderContext {
    pub args: ToolArgs,
    pub tool_call_id: String,
    pub invalidate: Box<dyn Fn() + Send + Sync>,
    pub last_component: Option<Box<dyn Component>>,
    pub state: HashMap<String, serde_json::Value>,
    pub cwd: String,
    pub execution_started: bool,
    pub args_complete: bool,
    pub is_partial: bool,
    pub expanded: bool,
    pub show_images: bool,
    pub is_error: bool,
    pub duration_ms: Option<u64>,
    pub output_pad: usize,
    pub calls: Option<Vec<NestedCall>>,
    pub ui: Box<dyn RenderContext>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NestedCall {
    pub id: String,
    pub name: String,
    pub args: ToolArgs,
    pub result: Option<ToolResultView>,
}

pub type NestedRenderer = std::rc::Rc<dyn Fn(&ToolView) -> Option<Vec<NestedCall>> + Send + Sync>;
pub type CallRenderer =
    std::rc::Rc<dyn Fn(&ToolArgs, &Theme, &ToolRenderContext) -> Box<dyn Component> + Send + Sync>;
pub type ResultRenderer = std::rc::Rc<
    dyn Fn(
            &ToolResultView,
            &ToolRenderResultOptions,
            &Theme,
            &ToolRenderContext,
        ) -> Box<dyn Component>
        + Send
        + Sync,
>;
#[derive(Clone, Default)]
pub struct ToolRenderers {
    pub render_shell: Option<String>,
    pub nested_calls: Option<NestedRenderer>,
    pub render_call: Option<CallRenderer>,
    pub render_result: Option<ResultRenderer>,
}

pub type ToolRendererResolver = Box<
    dyn Fn(
            &str,
            &dyn Fn() -> Option<ToolRenderers>,
            &crate::preferences::TuiSettings,
        ) -> Option<ToolRenderers>
        + Send
        + Sync,
>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolView {
    pub id: String,
    pub name: String,
    pub args: ToolArgs,
    pub status: ToolStatus,
    pub result: Option<ToolResultView>,
    pub calls: Option<Vec<NestedCall>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolStatus {
    Pending,
    Running,
    #[serde(rename = "done", alias = "success")]
    Success,
    Error,
    Interrupted,
}
