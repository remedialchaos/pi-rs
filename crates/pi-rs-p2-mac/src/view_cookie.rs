use crate::{
    Error, Result, http,
    local_sign_in::{WebCookie, response_cookie},
};
use std::time::Duration;
use url::Url;

pub const RENEW_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
pub const RETRY_AFTER: Duration = Duration::from_secs(5 * 60);

pub async fn exchange(grant: &Url) -> Result<WebCookie> {
    let query: Vec<_> = grant.query_pairs().filter(|(k, _)| k != "path").collect();
    let mut url = grant.clone();
    url.set_query(None);
    if !query.is_empty() {
        url.query_pairs_mut().extend_pairs(query);
    }
    let response = http(Duration::from_secs(15))?
        .get(url)
        .header("Sec-Fetch-Mode", "navigate")
        .header("Sec-Fetch-Dest", "document")
        .header("Sec-Fetch-Site", "none")
        .send()
        .await?;
    if response.status().as_u16() == 204
        && let Some(cookie) = response_cookie(&response, grant, |_| true)
    {
        return Ok(cookie);
    }
    Err(Error::Protocol(format!(
        "The Pi's origin set no view cookie ({})",
        response.status().as_u16()
    )))
}

/// The forwarded origin's expired navigation asks for an authorized grant instead.
pub fn forward_path(gateway: &Url, target: &Url) -> Option<String> {
    (target.path() == "/pi/forward" && crate::host_bridge::owns(gateway, target)).then(|| {
        target
            .query_pairs()
            .find(|(key, value)| key == "path" && value.starts_with('/'))
            .map_or_else(|| "/".into(), |(_, value)| value.into_owned())
    })
}
