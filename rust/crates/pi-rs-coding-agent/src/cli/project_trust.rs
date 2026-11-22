//! Restricted project trust context used before loading project-local resources.

use std::path::Path;

use serde_json::{Value, json};

use crate::core::extensions::{ExtensionMode, ExtensionUi};
use crate::core::project_trust::AppMode;

pub struct ProjectTrustContext<'a> {
    pub cwd: &'a Path,
    pub mode: ExtensionMode,
    pub has_ui: bool,
    startup_ui: Option<ExtensionUi>,
}

pub fn create_project_trust_context(
    cwd: &Path,
    mode: AppMode,
    has_ui: bool,
) -> ProjectTrustContext<'_> {
    ProjectTrustContext {
        cwd,
        mode: match mode {
            AppMode::Interactive => ExtensionMode::Tui,
            AppMode::Print => ExtensionMode::Print,
            AppMode::Json => ExtensionMode::Json,
            AppMode::Rpc => ExtensionMode::Rpc,
        },
        has_ui,
        startup_ui: None,
    }
}

impl ProjectTrustContext<'_> {
    pub fn with_startup_ui(mut self, startup_ui: ExtensionUi) -> Self {
        self.startup_ui = Some(startup_ui);
        self
    }

    /// The Rust event bus passes one JSON payload, including context metadata.
    pub fn extension_event(&self) -> Value {
        json!({
            "type": "project_trust",
            "cwd": self.cwd.to_string_lossy(),
            "mode": self.mode.as_str(),
            "hasUI": self.has_ui,
        })
    }

    fn interactive_ui(&self) -> Option<&ExtensionUi> {
        if self.has_ui && self.mode == ExtensionMode::Tui {
            self.startup_ui.as_ref()
        } else {
            None
        }
    }

    pub async fn select(&self, title: String, options: Vec<String>) -> Option<String> {
        self.interactive_ui()?.select(title, options).await
    }

    pub async fn confirm(&self, title: &str, message: &str) -> bool {
        self.select(
            format!("{title}\n{message}"),
            vec!["Yes".into(), "No".into()],
        )
        .await
        .as_deref()
            == Some("Yes")
    }

    pub async fn input(&self, title: String, placeholder: Option<String>) -> Option<String> {
        self.interactive_ui()?.input(title, placeholder).await
    }

    pub fn notify(&self, message: &str) {
        if self.mode != ExtensionMode::Tui {
            eprintln!("{message}");
        }
    }
}
