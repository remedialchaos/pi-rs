use std::time::Duration;

use super::settings_manager::SettingsManager;

/// Rust does not mutate the process environment. Settings fill in only the proxy
/// schemes not already defined by the environment, without changing NO_PROXY.
pub fn client(settings: &SettingsManager) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder().http1_only().user_agent(
        crate::utils::pi_user_agent::pi_user_agent(crate::config::VERSION),
    );
    let idle = settings.get_http_idle_timeout_ms()?;
    if idle > 0 {
        builder = builder.read_timeout(Duration::from_millis(idle as u64));
    }
    let global = settings.get_global_settings();
    if let Some(proxy) = global["httpProxy"]
        .as_str()
        .map(str::trim)
        .filter(|proxy| !proxy.is_empty())
    {
        let no_proxy = reqwest::NoProxy::from_env();
        let http = std::env::var("HTTP_PROXY")
            .or_else(|_| std::env::var("http_proxy"))
            .unwrap_or_else(|_| proxy.to_owned());
        let https = std::env::var("HTTPS_PROXY")
            .or_else(|_| std::env::var("https_proxy"))
            .unwrap_or_else(|_| proxy.to_owned());
        if !http.is_empty() {
            builder = builder.proxy(
                reqwest::Proxy::http(http)
                    .map_err(|error| error.to_string())?
                    .no_proxy(no_proxy.clone()),
            );
        }
        if !https.is_empty() {
            builder = builder.proxy(
                reqwest::Proxy::https(https)
                    .map_err(|error| error.to_string())?
                    .no_proxy(no_proxy),
            );
        }
    }
    builder.build().map_err(|error| error.to_string())
}
