//! The error types of the OAuth client subset (`packages/mcp/src/oauth/errors.ts`).

/// The errors of the OAuth client subset, one variant per error class upstream throws.
#[derive(Debug, Clone, PartialEq)]
pub enum OAuthError {
    /// `OAuthError`: the server answered with an OAuth error code.
    Protocol {
        /// The OAuth `error` code.
        code: String,
        message: String,
        error_uri: Option<String>,
    },
    /// `OAuthIssuerMismatchError`: `expected` names the metadata issuer, `received` the `iss`.
    IssuerMismatch {
        expected: Option<String>,
        received: Option<String>,
    },
    /// `OAuthInsecureEndpointError`.
    Insecure { endpoint: String },
    /// `OAuthRegistrationError`.
    Registration { status: u16, body: String },
    /// `McpOAuthAuthorizationRequiredError`.
    AuthorizationRequired,
    /// `McpRefreshLockBusyError`.
    RefreshLockBusy,
    /// `McpSignInCancelledError`.
    SignInCancelled,
    /// A network failure, the kind upstream rethrows from discovery as a `TypeError`.
    Network(String),
    /// A plain `Error`, by its message.
    Message(String),
}

impl std::fmt::Display for OAuthError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Protocol { message, .. } => formatter.write_str(message),
            Self::IssuerMismatch { expected, received } => {
                let format = |issuer: &Option<String>| match issuer {
                    Some(issuer) => format!("\"{issuer}\""),
                    None => "none".to_string(),
                };
                write!(
                    formatter,
                    "OAuth issuer mismatch: expected {}, received {}",
                    format(expected),
                    format(received)
                )
            }
            Self::Insecure { endpoint } => {
                write!(
                    formatter,
                    "Refusing to send OAuth credentials to non-HTTPS endpoint {endpoint}"
                )
            }
            Self::Registration { status, body } => {
                write!(
                    formatter,
                    "OAuth dynamic client registration failed with status {status}: {body}"
                )
            }
            Self::AuthorizationRequired => {
                formatter.write_str("MCP OAuth authorization requires user interaction")
            }
            Self::RefreshLockBusy => {
                formatter.write_str("Another process is refreshing the MCP server's tokens")
            }
            Self::SignInCancelled => formatter.write_str("Sign-in cancelled"),
            Self::Network(message) | Self::Message(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for OAuthError {}

impl From<crate::fetch::McpFetchError> for OAuthError {
    fn from(error: crate::fetch::McpFetchError) -> Self {
        match error {
            crate::fetch::McpFetchError::Network(message) => Self::Network(message),
            crate::fetch::McpFetchError::Timeout(message)
            | crate::fetch::McpFetchError::Other(message) => Self::Message(message),
        }
    }
}

impl OAuthError {
    /// The OAuth error code of a protocol failure, for the retry decisions.
    pub fn protocol_code(&self) -> Option<&str> {
        match self {
            Self::Protocol { code, .. } => Some(code),
            _ => None,
        }
    }
}
