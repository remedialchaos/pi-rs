use crate::{Error, Result, http};
use cookie::Cookie;
use hmac::{Hmac, Mac};
use reqwest::{Response, header};
use serde::Deserialize;
use serde_json::json;
use sha2::Sha256;
use std::{path::Path, time::Duration};
use url::Url;

pub struct WebCookie {
    pub cookie: Cookie<'static>,
    pub origin: Url,
    raw: String,
}
impl WebCookie {
    pub fn set_cookie_header(&self) -> &str {
        &self.raw
    }
}

pub(crate) fn response_cookie(
    response: &Response,
    origin: &Url,
    accept: impl Fn(&Cookie<'_>) -> bool,
) -> Option<WebCookie> {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|h| h.to_str().ok())
        .find_map(|raw| {
            let cookie = Cookie::parse(raw).ok()?;
            accept(&cookie).then(|| WebCookie {
                cookie: cookie.into_owned(),
                origin: origin.clone(),
                raw: raw.to_owned(),
            })
        })
}

pub fn proof(key: &str, port: u16, challenge: &str) -> String {
    // INFALLIBLE: HMAC-SHA256 accepts keys of any length.
    let mut mac =
        Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("HMAC accepts any key length");
    mac.update(b"p2-local-owner:");
    mac.update(port.to_string().as_bytes());
    mac.update(b":");
    mac.update(challenge.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

pub async fn cookie(origin: &Url, data_dir: &Path) -> Result<WebCookie> {
    if origin.scheme() != "http" || origin.host_str() != Some("127.0.0.1") {
        return Err(Error::Protocol(
            "local owner sign-in requires HTTP on 127.0.0.1".into(),
        ));
    }
    let file = data_dir.join("owner-key");
    let key = std::fs::read_to_string(&file)
        .map_err(|_| Error::Protocol(format!("Cannot read {}", file.display())))?;
    let origin_header = origin.origin().ascii_serialization();
    let port = origin.port().unwrap_or(80);
    let client = http(Duration::from_secs(5))?;
    let challenge = client
        .post(origin.join("/owner/local/challenge")?)
        .header(header::ORIGIN, &origin_header)
        .json(&json!({}))
        .send()
        .await?;
    #[derive(Deserialize)]
    struct Challenge {
        challenge: String,
    }
    let status = challenge.status().as_u16();
    let challenge = if status == 200 {
        challenge.json::<Challenge>().await.ok()
    } else {
        None
    };
    let Some(Challenge { challenge }) = challenge else {
        return Err(Error::Protocol(format!(
            "The server at {origin_header} offers no local sign-in ({status})"
        )));
    };
    let proof = proof(key.trim(), port, &challenge);
    let claim = client
        .post(origin.join("/owner/local/claim")?)
        .header(header::ORIGIN, &origin_header)
        .json(&json!({"challenge":challenge,"proof":proof}))
        .send()
        .await?;
    if claim.status().as_u16() == 403 {
        return Err(Error::Protocol(format!(
            "The server at {origin_header} does not use the data directory {}",
            data_dir.display()
        )));
    }
    let name = format!("p2_owner_{port}");
    response_cookie(&claim, origin, |cookie| {
        cookie.name() == name && cookie.http_only() == Some(true)
    })
    .ok_or_else(|| {
        Error::Protocol(format!(
            "Local sign-in at {origin_header} failed ({})",
            claim.status().as_u16()
        ))
    })
}
