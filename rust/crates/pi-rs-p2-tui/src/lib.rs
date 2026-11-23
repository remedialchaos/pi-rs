//! Terminal UI client for `p2` (Milestone 3.2-tui).
#![forbid(unsafe_code)]

pub mod completion;
pub mod components;
pub mod core_commands;
pub mod grant_notice;
pub mod host;
pub mod keybindings;
pub mod messages;
pub mod preferences;
pub mod render_context;
pub mod settings;
pub mod snapshot;
pub mod theme;
pub mod tools;

pub use completion::*;
pub use core_commands::*;
pub use grant_notice::*;
pub use keybindings::*;
pub use messages::*;
pub use preferences::*;
pub use render_context::*;
pub use settings::*;
pub use snapshot::*;
pub use theme::*;
