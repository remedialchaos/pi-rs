//! Native interactive services and presentation helpers.

mod delivery;
mod easter_egg;
mod editor;
mod models;
mod presentation;
mod status;

pub use delivery::{
    BUG_REPORT_DISCLAIMER, BUG_REPORT_TRANSCRIPT_NOTE, BugReportBundle, BugReportOptions,
    BugReportUploadOptions, ShareDestination, ShareLinks, ShareOptions, bug_report_gateway,
    build_bug_report, export_bug_report, radius_gateway, share_session, share_session_with_options,
    summarize_bug_report, upload_bug_report, upload_bug_report_with_options,
};
pub use easter_egg::{EasterEgg3d, EasterEggAnimation};
pub use editor::{external_editor, external_editor_with_command};
pub use models::{
    ModelCatalogRefresh, get_distinct_model_name, get_model_search_text,
    get_model_selector_search_text,
};
pub use presentation::{
    ArminAnimation, AuthUrl, CountdownTimer, EarendilAnnouncement, format_key_text,
    key_display_text, key_hint, key_text, pi_logo_lines, pi_wordmark, provider_brand, raw_key_hint,
    supports_pi_logo,
};
pub use status::{BlockedStatus, ProgramStatusReporter, dialog_blocked_status};
