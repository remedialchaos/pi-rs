//! The MCP OAuth client subset (`packages/mcp/src/oauth/index.ts`), without depending on the
//! official SDK: discovery, PKCE, dynamic client registration, the loopback callback server, the
//! two-half sign-in, and the connection auth provider.

pub mod auth_provider;
pub mod callback;
pub mod client_settings;
pub mod discovery;
pub mod errors;
pub mod flow;
pub mod loopback;
pub mod provider;
pub mod sign_in;
pub mod types;

pub use auth_provider::{
    McpAuthProvider, McpAuthProviderImpl, McpAuthProviderOptions, RefreshStoredTokensOptions,
    create_mcp_auth_provider, refresh_stored_tokens,
};
pub use callback::{
    OAuthCallback, OAuthCallbackPage, OAuthCallbackServer, OAuthCallbackServerOptions, RenderPage,
};
pub use client_settings::{
    ClientRegistration, DEFAULT_REDIRECT_URL, LockFn, LoopbackSettings, McpClientMetadataDocuments,
    McpOAuthClientOptions, McpOAuthServerStore, McpOAuthSettings, client_registration_as_string,
    client_registration_from_value, create_oauth_client_provider, loopback_settings,
    offers_client_metadata_document, registered_redirect_urls, server_specific_path, timed_fetch,
};
pub use discovery::{
    DiscoveryKind, DiscoveryOptions, build_authorization_server_discovery_urls,
    discover_authorization_server_metadata, discover_oauth_server_info,
    discover_protected_resource_metadata, parse_www_authenticate, resource_url_from_server_url,
    select_resource,
};
pub use errors::OAuthError;
pub use flow::{
    AddClientAuthentication, AuthorizationRequest, ClientMetadataDocument, InvalidateKind,
    OAuthClientProvider, OAuthFlowOptions, OAuthFlowResult, RegisterClientOptions,
    StartAuthorizationOptions, StoreFuture, TokenRequestOptions, adapt_oauth_provider,
    authorize_mcp, exchange_authorization_code, refresh_authorization, register_client,
    start_authorization, step_up_scope,
};
pub use loopback::{
    LoopbackSignInOptions, McpLoopbackCallback, PromptForRedirectUrl, WaitForLoopbackOptions,
    listen_for_loopback_sign_in, response_from_redirect_url, wait_for_loopback_sign_in,
};
pub use provider::{
    ClientMetadataDocumentHook, McpOAuthProvider, McpOAuthProviderOptions, McpOAuthState,
    McpOAuthStateStore, McpStoreError, MemoryOAuthStateStore, OnRedirect,
};
pub use sign_in::{
    McpSignInOptions, McpSignInRedirect, McpSignInStart, complete_mcp_sign_in, start_mcp_sign_in,
};
pub use types::{
    AuthorizationServerMetadata, OAuthChallenge, OAuthClientInformation, OAuthClientMetadata,
    OAuthClientMetadataDocument, OAuthDiscoveryState, OAuthProtectedResourceMetadata,
    OAuthServerInfo, OAuthTokens, parse_authorization_server_metadata, parse_client_information,
    parse_oauth_tokens, parse_protected_resource_metadata,
};
