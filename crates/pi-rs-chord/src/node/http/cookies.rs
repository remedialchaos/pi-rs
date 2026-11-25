use super::{HttpError, HttpResult, percent_decode, percent_encode};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use sha2::Sha256;

#[derive(Clone, Copy, Debug, Default)]
pub enum SameSite {
    #[default]
    Strict,
    Lax,
}
#[derive(Clone, Debug)]
pub struct CookieOptions {
    pub secret: String,
    pub secure: bool,
    pub max_age: Option<i64>,
    pub same_site: SameSite,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedCookie {
    pub value: String,
    pub expires: Option<u64>,
}

pub fn cookie(name: &str, value: &str, options: &CookieOptions) -> HttpResult<String> {
    cookie_at(name, value, options, super::now_seconds())
}
pub fn cookie_at(name: &str, value: &str, options: &CookieOptions, now: u64) -> HttpResult<String> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
    {
        return Err(HttpError::new(500, "invalid cookie name"));
    }
    let expires = options
        .max_age
        .map(|age| (i128::from(now) + i128::from(age)).to_string())
        .unwrap_or_default();
    let payload = format!("{}.{}", percent_encode(value), expires);
    let mut mac = Hmac::<Sha256>::new_from_slice(options.secret.as_bytes())
        .map_err(|_| HttpError::new(500, "invalid signing key"))?;
    mac.update(format!("{name}={payload}").as_bytes());
    let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    let site = match options.same_site {
        SameSite::Strict => "Strict",
        SameSite::Lax => "Lax",
    };
    let mut value = format!("{name}={payload}.{signature}; HttpOnly; Path=/; SameSite={site}");
    if options.secure {
        value.push_str("; Secure");
    }
    if let Some(age) = options.max_age {
        value.push_str(&format!("; Max-Age={age}"));
    }
    Ok(value)
}
pub fn read_cookie(header: &str, name: &str, secret: &str) -> Option<String> {
    read_cookie_at(header, name, secret, super::now_seconds())
}
pub fn read_cookie_at(header: &str, name: &str, secret: &str, now: u64) -> Option<String> {
    read_signed_cookie_at(header, name, secret, now).map(|c| c.value)
}
pub fn read_signed_cookie_at(
    header: &str,
    name: &str,
    secret: &str,
    now: u64,
) -> Option<SignedCookie> {
    let signed = header.split(';').find_map(|pair| {
        let (key, value) = pair.trim().split_once('=')?;
        (key == name).then_some(value.trim())
    })?;
    let (payload, given) = signed.rsplit_once('.')?;
    let (value, stamp) = payload.rsplit_once('.')?;
    let given = URL_SAFE_NO_PAD.decode(given).ok()?;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(format!("{name}={payload}").as_bytes());
    mac.verify_slice(&given).ok()?;
    let expires = if stamp.is_empty() {
        None
    } else {
        Some(stamp.parse::<u64>().ok()?)
    };
    if expires.is_some_and(|expires| expires <= now) {
        return None;
    }
    Some(SignedCookie {
        value: percent_decode(value).ok()?,
        expires,
    })
}
