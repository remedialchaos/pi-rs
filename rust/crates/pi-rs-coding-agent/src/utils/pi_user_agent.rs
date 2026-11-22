//! The `User-Agent` pi uses for its own network requests. Ported from
//! `utils/pi-user-agent.ts` on top of `pi_rs_ai::format_user_agent`.
//!
//! Upstream always sends the detailed form; the simple form (`pi`) is exposed
//! for callers that need a stable, minimal identifier.

use pi_rs_ai::http::format_user_agent;
use pi_rs_ai::{ClientIdentity, UserAgent};

use crate::config::VERSION;

/// Which `User-Agent` form to emit.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum UserAgentStyle {
    /// `pi/<version> (<os>; rust[/thread]; <arch>)`
    #[default]
    Detailed,
    /// `pi`
    Simple,
}

impl UserAgentStyle {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Detailed => "detailed",
            Self::Simple => "simple",
        }
    }
}

impl From<&str> for UserAgentStyle {
    fn from(value: &str) -> Self {
        match value {
            "simple" => Self::Simple,
            _ => Self::Detailed,
        }
    }
}

/// Options for [`get_pi_user_agent`].
#[derive(Clone, Debug, PartialEq)]
pub struct PiUserAgentOptions {
    pub user_agent: UserAgentStyle,
    pub version: String,
    /// Optional thread name inserted into the detailed runtime token.
    pub thread_name: Option<String>,
    /// Optional thread id, used when no name is available.
    pub thread_id: Option<String>,
}

impl Default for PiUserAgentOptions {
    fn default() -> Self {
        Self {
            user_agent: UserAgentStyle::Detailed,
            version: VERSION.to_string(),
            thread_name: None,
            thread_id: None,
        }
    }
}

impl PiUserAgentOptions {
    pub fn new(version: impl Into<String>) -> Self {
        Self {
            version: version.into(),
            ..Self::default()
        }
    }
}

/// Runtime token used in the detailed form. The suffix is derived from explicit
/// options so the output is deterministic; `default_thread_options` can capture
/// the current thread.
pub fn runtime_token(options: &PiUserAgentOptions) -> String {
    match (&options.thread_name, &options.thread_id) {
        (Some(name), _) if !name.is_empty() => format!("rust/{name}"),
        (None, Some(id)) if !id.is_empty() => format!("rust/{id}"),
        _ => "rust".to_string(),
    }
}

/// Fill thread fields from the current thread when available.
pub fn default_thread_options(version: &str) -> PiUserAgentOptions {
    let thread = std::thread::current();
    PiUserAgentOptions {
        version: version.to_string(),
        thread_name: thread.name().map(str::to_string),
        thread_id: Some(format!("{:?}", thread.id())),
        ..PiUserAgentOptions::default()
    }
}

/// Format the user agent. The detailed form matches the shape pi-ai sends to
/// providers: `pi/1.2.3 (linux; rust; x86_64)`.
pub fn get_pi_user_agent(version: &str, options: &PiUserAgentOptions) -> String {
    if options.user_agent == UserAgentStyle::Simple {
        return "pi".to_string();
    }
    let identity = ClientIdentity {
        user_agent: Some(UserAgent::Custom(format!(
            "pi/{version} ({}; {}; {})",
            std::env::consts::OS,
            runtime_token(options),
            std::env::consts::ARCH
        ))),
        version: Some(version.to_string()),
        attribution: None,
    };
    format_user_agent(&identity)
}

/// Convenience wrapper using the default detailed options.
pub fn pi_user_agent(version: &str) -> String {
    get_pi_user_agent(version, &PiUserAgentOptions::new(version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detailed_form_matches_upstream_shape() {
        let agent = get_pi_user_agent("1.2.3", &PiUserAgentOptions::new("1.2.3"));
        assert_eq!(
            agent,
            format!(
                "pi/1.2.3 ({}; rust; {})",
                std::env::consts::OS,
                std::env::consts::ARCH
            )
        );
    }

    #[test]
    fn detailed_form_matches_upstream_regex() {
        let agent = get_pi_user_agent("1.2.3", &PiUserAgentOptions::new("1.2.3"));
        let parts: Vec<&str> = agent.split([';', '(', ')']).collect();
        assert_eq!(parts.len(), 5);
        assert!(parts[0].starts_with("pi/1.2.3 "));
        assert_eq!(parts[3].trim(), std::env::consts::ARCH);
    }

    #[test]
    fn simple_form_is_bare_pi() {
        let options = PiUserAgentOptions {
            user_agent: UserAgentStyle::Simple,
            ..PiUserAgentOptions::new("1.2.3")
        };
        assert_eq!(get_pi_user_agent("1.2.3", &options), "pi");
        assert_eq!(UserAgentStyle::from("simple"), UserAgentStyle::Simple);
        assert_eq!(UserAgentStyle::from("detailed"), UserAgentStyle::Detailed);
    }

    #[test]
    fn thread_info_is_included_when_supplied() {
        let options = PiUserAgentOptions {
            thread_name: Some("main".into()),
            ..PiUserAgentOptions::new("2.0.0")
        };
        let agent = get_pi_user_agent("2.0.0", &options);
        assert_eq!(
            agent,
            format!(
                "pi/2.0.0 ({}; rust/main; {})",
                std::env::consts::OS,
                std::env::consts::ARCH
            )
        );
        assert_eq!(runtime_token(&options), "rust/main");
    }

    #[test]
    fn convenience_helper_uses_supplied_version() {
        assert!(pi_user_agent("9.9.9").starts_with("pi/9.9.9 ("));
    }
}
