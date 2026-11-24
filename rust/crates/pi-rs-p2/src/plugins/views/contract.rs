//! Views contracts, schemas, and cross-plugin `MessageContexts` service.

use std::sync::{Arc, Mutex};

use indexmap::IndexMap;
use pi_rs_chord::{
    Result,
    contracts::Contract,
    host::{Ctx, Host},
    sync::SyncFuture,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    api::v1::{
        p2_contracts::lookup,
        server::{install_service, service_of},
    },
    plugins::chat_mode::SendRouterInput,
};

/// The transcript's ID where annotations name the view they mark up.
pub const TRANSCRIPT: &str = "transcript";
/// How many annotations a session keeps at once.
pub const MAX_ANNOTATIONS: usize = 200;
/// The built-in kinds: a Markdown file of the workspace rendered, the working tree's changes.
pub const BUILT_IN_KINDS: &[&str] = &["markdown", "changes"];
/// The entry kind of a person's message that carried view context (selections, annotations) to the agent.
pub const VIEW_INPUT: &str = "p2.view-input";

pub const MESSAGE_CONTEXTS_SERVICE: &str = "p2.message-contexts";

/// Validates `^[a-z0-9][a-z0-9_-]{0,63}$`.
pub fn is_valid_view_id(id: &str) -> bool {
    let mut chars = id.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    id.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Validates `^[a-z0-9][a-z0-9._-]{0,63}$`.
pub fn is_valid_view_kind(kind: &str) -> bool {
    let mut chars = kind.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    kind.len() <= 64
        && chars.all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_' || c == '-'
        })
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Selection {
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sent: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub id: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection: Option<Selection>,
    pub focus: u64,
    pub created: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum AnnotationInput {
    Comment {
        view: String,
        quote: String,
        note: String,
    },
    Delete {
        view: String,
        quote: String,
    },
    Ink {
        view: String,
        strokes: Vec<Vec<f64>>,
        aspect: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        under: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
}

impl AnnotationInput {
    pub fn view(&self) -> &str {
        match self {
            Self::Comment { view, .. } | Self::Delete { view, .. } | Self::Ink { view, .. } => view,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Annotation {
    pub id: String,
    pub kind: String,
    pub view: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strokes: Option<Vec<Vec<f64>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aspect: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub under: Option<String>,
    pub author: String,
    pub created: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sent: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Arrangement {
    pub layout: Value,
    pub stamp: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InlineSelection {
    pub kind: String,
    pub item: String,
    pub title: String,
    pub selection: Selection,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectInlineInput {
    pub kind: String,
    pub item: String,
    pub title: String,
    pub selection: Option<Selection>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewsState {
    #[serde(default)]
    pub views: Vec<View>,
    #[serde(default)]
    pub annotations: Vec<Annotation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arrangement: Option<Arrangement>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inline: Option<Vec<InlineSelection>>,
}

/// The views of `state` that still exist: every kind except the removed `custom` one.
pub fn live_views(state: &ViewsState) -> Vec<View> {
    state
        .views
        .iter()
        .filter(|view| view.kind != "custom")
        .cloned()
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenInput {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Context another plugin adds to a person's next message beside views' selections/annotations.
pub struct MessageContext {
    pub text: String,
    pub note: String,
    pub carried: Box<dyn FnOnce(Ctx) -> SyncFuture<()> + Send>,
}

pub type MessageContextFn =
    Arc<dyn Fn(SendRouterInput, Ctx) -> SyncFuture<Option<MessageContext>> + Send + Sync>;

#[derive(Default)]
pub struct MessageContextsService {
    providers: Mutex<IndexMap<String, MessageContextFn>>,
}

impl MessageContextsService {
    pub fn contribute(&self, name: impl Into<String>, provider: MessageContextFn) {
        self.providers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.into(), provider);
    }

    pub fn providers(&self) -> Vec<MessageContextFn> {
        self.providers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }
}

pub fn get_or_install_message_contexts(host: &Host) -> Result<Arc<MessageContextsService>> {
    match service_of::<MessageContextsService>(host, MESSAGE_CONTEXTS_SERVICE) {
        Ok(existing) => Ok(existing),
        Err(_) => {
            let service = Arc::new(MessageContextsService::default());
            install_service(host, MESSAGE_CONTEXTS_SERVICE, &service)?;
            Ok(service)
        }
    }
}

pub fn views_contract() -> Result<Arc<Contract>> {
    Ok(Arc::new(lookup("p2.views")?))
}
