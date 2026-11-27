//! The sign-in plugin: who someone is. A sign-in is one browser or device signed in as a principal,
//! behind a token `<id>.<secret>` (random); only the secret's hash is kept, compared in constant
//! time. The owner's one-time link (single use, 10 minutes) signs a browser in; the owner token file
//! (a device sign-in, mode 0600) in the data directory is what `p3` asks for a fresh link with.
//!
//! Upstream keeps the sign-ins in the server's Chord store as documents and serves `SignIns` as a
//! remote object. This port keeps them in the Pi's own private document (`sign-ins.json`); the
//! object's typed entry points live on [`SignIns`]. See `MILESTONE.md`.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use http::HeaderValue;
use indexmap::IndexMap;
use pi_rs_chord::{
    host::{Facet, define_facet},
    node::app::{new_token, write_secret},
    node::http::{
        Http, HttpContribution, HttpFuture, HttpRequest, HttpResult, HttpWrap, Provenance,
        RouteOptions, WrapPhase, cookie, now_seconds, read_cookie, read_signed_cookie_at,
    },
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    api::v1::contribute::PluginLives,
    files::write_private,
    http::{error, html, json, principal, set_header},
    principals::Principal,
    registry::{LinkOpenedHook, ServerRegistries},
    server::ServerRuntime,
};

/// How long an owner link is valid: 10 minutes.
pub const OWNER_LINK_MS: u64 = 10 * 60_000;

/// The owner link's page and post.
pub const OWNER_PATH: &str = "/_p3/owner";

/// The path a signed-in owner posts to for a fresh owner link.
pub const OWNER_LINK_PATH: &str = "/_p3/owner/link";

/// How long a sign-in lives unused: 400 days, as its cookie.
pub const SIGN_IN_IDLE_MS: u64 = 400 * 24 * 60 * 60_000;

/// How often a sign-in's `lastUsed` is written at most: once a minute.
const TOUCH_MS: u64 = 60_000;

/// How old a sign-in cookie gets before a response renews it: a day.
const ROLL_MS: u64 = 24 * 60 * 60_000;

/// The owner token file in a data directory.
pub fn owner_token_file(data_dir: &Path) -> PathBuf {
    data_dir.join("owner-token")
}

/// What a sign-in is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SignInKind {
    Browser,
    Device,
}

/// A sign-in as the owner may see it: never a token or a hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignInView {
    pub id: String,
    pub label: String,
    pub kind: SignInKind,
    pub principal: Principal,
    pub created: u64,
    pub last_used: u64,
}

/// A new sign-in: its id, its token (shown once, kept only hashed) and its cookie.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issued {
    pub id: String,
    pub token: String,
    pub cookie: String,
}

/// What the sign-in store refused.
#[derive(Debug, thiserror::Error)]
pub enum SignInError {
    #[error("sign-ins: {0}")]
    Io(#[from] std::io::Error),
    #[error("sign-ins: {0}")]
    Corrupt(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct StoredSignIn {
    hash: String,
    label: String,
    kind: SignInKind,
    principal: Principal,
    created: u64,
    last_used: u64,
}

/// The cookie's key, made once per Pi: its id names the cookie (a browser shares cookies between
/// the ports of one host, so two Pis on a machine need two names), its secret signs it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct CookieKey {
    id: String,
    secret: String,
}

#[derive(Serialize, Deserialize)]
struct Document {
    sign_ins: IndexMap<String, StoredSignIn>,
    cookie: CookieKey,
}

struct Inner {
    document: Document,
    links: IndexMap<String, Vec<pi_rs_chord::node::http::LinkCloser>>,
}

#[derive(Clone)]
struct AuthenticatedSignIn(String);

/// The server's sign-ins: issue, verify, revoke, and the list of them.
pub struct SignIns {
    path: PathBuf,
    inner: Mutex<Inner>,
}

impl SignIns {
    /// The sign-ins of the Pi on `data_dir`, made when the file is missing.
    pub fn open(data_dir: &Path) -> Result<Arc<Self>, SignInError> {
        let path = data_dir.join("sign-ins.json");
        let document = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|error| SignInError::Corrupt(error.to_string()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Document {
                sign_ins: IndexMap::new(),
                cookie: CookieKey {
                    id: new_token(8),
                    secret: new_token(32),
                },
            },
            Err(error) => return Err(error.into()),
        };
        let sign_ins = Arc::new(Self {
            path,
            inner: Mutex::new(Inner {
                document,
                links: IndexMap::new(),
            }),
        });
        if sign_ins.persist().is_err() {
            // A store that cannot be written still authenticates within this run; the next start
            // makes a fresh one. The owner token file then replaces its sign-in.
        }
        Ok(sign_ins)
    }

    /// The sign-in cookie's name and secret.
    pub fn cookie_key(&self) -> (String, String) {
        let inner = self.lock();
        (
            format!("p3-{}", inner.document.cookie.id),
            inner.document.cookie.secret.clone(),
        )
    }

    /// Issues a sign-in for `principal`; `secure` marks the cookie `Secure` for a remote browser.
    pub fn issue(
        &self,
        principal: &Principal,
        label: &str,
        kind: SignInKind,
        secure: bool,
    ) -> Result<Issued, SignInError> {
        let id = new_token(12);
        let secret = new_token(32);
        let now = now_ms();
        let stored = StoredSignIn {
            hash: hash_token(&secret),
            label: label.to_string(),
            kind,
            principal: principal.clone(),
            created: now,
            last_used: now,
        };
        let (name, key) = {
            let mut inner = self.lock();
            inner.document.sign_ins.insert(id.clone(), stored);
            (
                format!("p3-{}", inner.document.cookie.id),
                inner.document.cookie.secret.clone(),
            )
        };
        self.persist()?;
        let token = format!("{id}.{secret}");
        let cookie = cookie(
            &name,
            &token,
            &pi_rs_chord::node::http::CookieOptions {
                secret: key,
                secure,
                max_age: Some((SIGN_IN_IDLE_MS / 1000) as i64),
                same_site: pi_rs_chord::node::http::SameSite::Strict,
            },
        )
        .map_err(|error| SignInError::Io(std::io::Error::other(error.to_string())))?;
        Ok(Issued { id, token, cookie })
    }

    /// The principal `token` authenticates as, or `None` (unknown, revoked, expired, malformed).
    pub fn verify(&self, token: &str) -> Option<Principal> {
        self.verify_with_id(token).map(|(_, principal)| principal)
    }

    /// The sign-in `token` names and the principal it authenticates as.
    pub fn verify_with_id(&self, token: &str) -> Option<(String, Principal)> {
        let id = sign_in_of(token)?.to_string();
        let secret = token.get(id.len() + 1..)?;
        let now = now_ms();
        let (matched, last_used, principal) = {
            let inner = self.lock();
            let stored = inner.document.sign_ins.get(&id)?;
            (
                matches_hash(secret, &stored.hash),
                stored.last_used,
                stored.principal.clone(),
            )
        };
        if !matched {
            return None;
        }
        if now.saturating_sub(last_used) > SIGN_IN_IDLE_MS {
            let _ = self.revoke(&id);
            return None;
        }
        if now.saturating_sub(last_used) > TOUCH_MS {
            {
                let mut inner = self.lock();
                if let Some(stored) = inner.document.sign_ins.get_mut(&id) {
                    stored.last_used = now;
                }
            }
            let _ = self.persist();
        }
        Some((id, principal))
    }

    /// Ends sign-in `id` at once: its token authenticates nothing more.
    pub fn revoke(&self, id: &str) -> bool {
        let mut inner = self.lock();
        if inner.document.sign_ins.shift_remove(id).is_none() {
            return false;
        }
        let links = inner.links.shift_remove(id).unwrap_or_default();
        drop(inner);
        let _ = self.persist();
        for close in links {
            close();
        }
        true
    }

    fn opened(
        self: &Arc<Self>,
        id: String,
        close: pi_rs_chord::node::http::LinkCloser,
    ) -> Option<Box<dyn FnOnce() + Send>> {
        let mut inner = self.lock();
        if !inner
            .document
            .sign_ins
            .get(&id)
            .is_some_and(|entry| now_ms().saturating_sub(entry.last_used) <= SIGN_IN_IDLE_MS)
        {
            drop(inner);
            close();
            return None;
        }
        inner
            .links
            .entry(id.clone())
            .or_default()
            .push(close.clone());
        let store = self.clone();
        Some(Box::new(move || {
            let mut inner = store.lock();
            if let Some(links) = inner.links.get_mut(&id) {
                links.retain(|link| !Arc::ptr_eq(link, &close));
                if links.is_empty() {
                    inner.links.shift_remove(&id);
                }
            }
        }))
    }

    fn renew_cookie(
        &self,
        header: &str,
        name: &str,
        secret: String,
        provenance: &Provenance,
    ) -> HttpResult<Option<String>> {
        let now = now_seconds();
        let Some(signed) = read_signed_cookie_at(header, name, &secret, now) else {
            return Ok(None);
        };
        if signed
            .expires
            .is_some_and(|expires| expires.saturating_sub(now) > (SIGN_IN_IDLE_MS - ROLL_MS) / 1000)
            || self.verify(&signed.value).is_none()
        {
            return Ok(None);
        }
        cookie(
            name,
            &signed.value,
            &pi_rs_chord::node::http::CookieOptions {
                secret,
                secure: provenance != &Provenance::Local,
                max_age: Some((SIGN_IN_IDLE_MS / 1000) as i64),
                same_site: pi_rs_chord::node::http::SameSite::Strict,
            },
        )
        .map(Some)
    }

    /// The sign-ins, never a token or a hash.
    pub fn list(&self) -> Vec<SignInView> {
        self.lock()
            .document
            .sign_ins
            .iter()
            .map(|(id, stored)| SignInView {
                id: id.clone(),
                label: stored.label.clone(),
                kind: stored.kind,
                principal: stored.principal.clone(),
                created: stored.created,
                last_used: stored.last_used,
            })
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn persist(&self) -> Result<(), SignInError> {
        let text = {
            let inner = self.lock();
            serde_json::to_string(&inner.document)
                .map_err(|error| SignInError::Corrupt(error.to_string()))?
        };
        write_private(&self.path, &text)
            .map_err(|error| SignInError::Io(std::io::Error::other(error)))
    }
}

/// Keeps the owner token file: written with a device sign-in of the owner while its token
/// authenticates as the owner, else replaced with a new one.
pub fn ensure_owner_token(path: &Path, sign_ins: &SignIns) -> Result<(), SignInError> {
    if let Ok(text) = std::fs::read_to_string(path) {
        let token = text.trim().to_string();
        if sign_ins
            .verify(&token)
            .is_some_and(|principal| principal.is_owner())
        {
            set_owner_token_mode(path)?;
            return Ok(());
        }
    }
    let issued = sign_ins.issue(
        &Principal::Owner,
        "owner token file",
        SignInKind::Device,
        true,
    )?;
    write_secret(path, &issued.token, true)
        .map_err(|error| SignInError::Io(std::io::Error::other(error.to_string())))?;
    Ok(())
}

fn set_owner_token_mode(path: &Path) -> Result<(), SignInError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    let _ = path;
    Ok(())
}

/// The owner link and its one-time token: made on request, consumed by its first use.
#[derive(Default)]
pub struct OwnerLink {
    current: Mutex<Option<(String, u64)>>,
}

impl OwnerLink {
    /// A fresh link at `origin` (the previous one no longer valid), valid 10 minutes.
    pub fn renew(&self, origin: &str) -> String {
        let token = new_token(32);
        *self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((hash_token(&token), now_ms() + OWNER_LINK_MS));
        format!("{origin}{OWNER_PATH}#{token}")
    }

    /// Whether `token` is the current one and has not expired; it is used up either way when it
    /// matches.
    pub fn consume(&self, token: &str) -> bool {
        let mut current = self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some((hash, expires)) = current.clone() else {
            return false;
        };
        if !matches_hash(token, &hash) {
            return false;
        }
        *current = None;
        now_ms() < expires
    }
}

/// A fresh random token of `bytes` bytes, base64url.
pub fn new_secret(bytes: usize) -> String {
    new_token(bytes)
}

/// The hash a token is kept as (SHA-256, hex): what leaks from a store is no credential.
pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Whether `token` hashes to `hash`, compared in constant time.
pub fn matches_hash(token: &str, hash: &str) -> bool {
    let given = hash_token(token);
    if given.len() != hash.len() {
        return false;
    }
    given
        .bytes()
        .zip(hash.bytes())
        .fold(0u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

/// The id of the sign-in a token `<id>.<secret>` names, or `None` for a text that is no token.
fn sign_in_of(token: &str) -> Option<&str> {
    let (id, _) = token.split_once('.')?;
    (!id.is_empty()).then_some(id)
}

/// The token a request carries: its bearer token first, else its sign-in cookie.
fn request_token(request: &HttpRequest, key: &(String, String)) -> Option<(String, bool)> {
    if let Some(header) = request.header("authorization")
        && let Some(token) = bearer(header)
    {
        return Some((token.to_string(), true));
    }
    let header = request.header("cookie")?;
    read_cookie(header, &key.0, &key.1).map(|token| (token, false))
}

fn bearer(header: &str) -> Option<&str> {
    let mut parts = header.split_whitespace();
    let scheme = parts.next()?;
    let token = parts.next()?;
    if parts.next().is_some() || !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    Some(token)
}

/// The sign-in plugin on the server: the sign-ins, their authenticator, the rolling-cookie wrap, and
/// the owner's link and token file.
pub fn server_facet(
    sign_ins: Arc<SignIns>,
    owner_link: Arc<OwnerLink>,
    http: Http,
    runtime: Arc<ServerRuntime>,
    registries: ServerRegistries,
    lives: &Arc<PluginLives>,
) -> Facet {
    let lives = lives.clone();
    define_facet(move |_host| -> pi_rs_chord::Result<()> {
        let live = lives.open("p3-sign-in");
        if let Err(error) = ensure_owner_token(&owner_token_file(runtime.data_dir()), &sign_ins) {
            eprintln!("p3 sign-in: the owner token file: {error}");
        }
        let (cookie_name, cookie_secret) = sign_ins.cookie_key();
        let mut contribution = HttpContribution::new("p3-sign-in");
        contribution
            .route(
                &format!("GET {OWNER_PATH}"),
                RouteOptions::public(|request| {
                    Box::pin(async move { html(owner_page(&request.nonce)) })
                }),
            )
            .map_err(|error| pi_rs_chord::Error::Invalid(error.to_string()))?;
        {
            let sign_ins = sign_ins.clone();
            let owner_link = owner_link.clone();
            contribution
                .route(
                    &format!("POST {OWNER_PATH}"),
                    RouteOptions::public(move |request| {
                        let sign_ins = sign_ins.clone();
                        let owner_link = owner_link.clone();
                        Box::pin(async move { post_owner(&request, &sign_ins, &owner_link).await })
                    }),
                )
                .map_err(|error| pi_rs_chord::Error::Invalid(error.to_string()))?;
        }
        {
            let owner_link = owner_link.clone();
            let runtime = runtime.clone();
            contribution
                .route(
                    &format!("POST {OWNER_LINK_PATH}"),
                    RouteOptions::new(move |request| {
                        let owner_link = owner_link.clone();
                        let runtime = runtime.clone();
                        Box::pin(async move {
                            if !principal(&request).is_some_and(|principal| principal.is_owner()) {
                                return error(403, "Forbidden");
                            }
                            let Some(port) = runtime.listening() else {
                                return error(503, "the server is not listening");
                            };
                            let link =
                                owner_link.renew(&format!("http://127.0.0.1:{}", port.port()));
                            json(200, &serde_json::json!({ "link": link }))
                        })
                    }),
                )
                .map_err(|error| pi_rs_chord::Error::Invalid(error.to_string()))?;
        }
        {
            let sign_ins = sign_ins.clone();
            let (name, secret) = (cookie_name.clone(), cookie_secret.clone());
            contribution
                .authenticators
                .push(Arc::new(move |request: HttpRequest| {
                    let sign_ins = sign_ins.clone();
                    let key = (name.clone(), secret.clone());
                    let future: HttpFuture<Option<serde_json::Value>> = Box::pin(async move {
                        let Some((token, _bearer)) = request_token(&request, &key) else {
                            return Ok(None);
                        };
                        Ok(sign_ins.verify_with_id(&token).map(|(id, principal)| {
                            request
                                .extensions
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .insert(AuthenticatedSignIn(id));
                            principal.value()
                        }))
                    });
                    future
                }));
        }
        {
            let sign_ins = sign_ins.clone();
            let (name, secret) = (cookie_name.clone(), cookie_secret.clone());
            let mut wrap = HttpWrap::new(
                "p3.rolling-cookie",
                WrapPhase::Headers,
                move |request, next| {
                    let sign_ins = sign_ins.clone();
                    let (name, secret) = (name.clone(), secret.clone());
                    Box::pin(async move {
                        let answer = next(request.clone()).await?;
                        let sets_cookie = answer
                            .headers()
                            .keys()
                            .any(|header| header.as_str().eq_ignore_ascii_case("set-cookie"));
                        if request.principal.is_none() || sets_cookie {
                            return Ok(answer);
                        }
                        let Some(header) = request.header("cookie") else {
                            return Ok(answer);
                        };
                        let Some(renewed) =
                            sign_ins.renew_cookie(header, &name, secret, &request.provenance)?
                        else {
                            return Ok(answer);
                        };
                        let mut answer = answer;
                        answer.headers_mut().insert(
                            "set-cookie",
                            HeaderValue::from_str(&renewed).map_err(|_| {
                                pi_rs_chord::node::http::HttpError::new(500, "invalid cookie")
                            })?,
                        );
                        Ok(answer)
                    })
                },
            );
            wrap.all = true;
            contribution.wraps.push(wrap);
        }
        {
            let sign_ins = sign_ins.clone();
            let hook: LinkOpenedHook = Arc::new(move |request, close| {
                let sign_in = request
                    .extensions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get::<AuthenticatedSignIn>()?
                    .clone();
                sign_ins.opened(sign_in.0, close)
            });
            live.contribute(&registries.link_opened, hook);
        }
        http.contribute(contribution)
            .map_err(|error| pi_rs_chord::Error::Invalid(error.to_string()))?;
        {
            let http = http.clone();
            live.cleanup(move || http.withdraw("p3-sign-in"));
        }
        Ok(())
    })
}

async fn post_owner(
    request: &HttpRequest,
    sign_ins: &SignIns,
    owner_link: &OwnerLink,
) -> HttpResult<pi_rs_chord::node::http::HttpResponse> {
    let body = request.json(64 * 1024).await?;
    // A trust boundary: anyone may post here.
    let token = body.get("token").and_then(|token| token.as_str());
    let Some(_) = token.filter(|token| owner_link.consume(token)) else {
        return error(403, "Forbidden");
    };
    let secure = request.provenance != Provenance::Local;
    let issued = match sign_ins.issue(
        &Principal::Owner,
        "owner browser",
        SignInKind::Browser,
        secure,
    ) {
        Ok(issued) => issued,
        Err(error) => return crate::http::error(500, &error.to_string()),
    };
    let mut answer = json(200, &serde_json::json!("signed in"))?;
    set_header(&mut answer, "set-cookie", &issued.cookie)?;
    Ok(answer)
}

/// The owner link's page: its script (the response's nonce) posts the fragment's token, then opens
/// p3.
fn owner_page(nonce: &str) -> String {
    [
        "<!doctype html><title>Sign in</title><p id=status>Signing in&hellip;</p>".to_string(),
        format!("<script nonce=\"{nonce}\">"),
        "const token = location.hash.slice(1); history.replaceState(null, '', location.pathname);"
            .to_string(),
        format!(
            "fetch({}, {{ method: 'POST', headers: {{ 'content-type': 'application/json' }}, body: JSON.stringify({{ token }}) }})",
            serde_json::to_string(OWNER_PATH).unwrap_or_else(|_| "\"/_p3/owner\"".into())
        ),
        "  .then((r) => { if (r.ok) location.replace('/'); else document.getElementById('status').textContent = 'This link is used up or expired.'; });"
            .to_string(),
        "</script>".to_string(),
    ]
    .join("\n")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign_ins() -> (tempfile::TempDir, Arc<SignIns>) {
        let root = tempfile::tempdir().expect("temp dir");
        let sign_ins = SignIns::open(root.path()).expect("open");
        (root, sign_ins)
    }

    #[test]
    fn rolling_an_aging_remote_cookie_preserves_secure() -> Result<(), Box<dyn std::error::Error>> {
        use pi_rs_chord::node::http::{CookieOptions, SameSite, cookie_at};
        let (_root, store) = sign_ins();
        let issued = store.issue(&Principal::Owner, "browser", SignInKind::Browser, true)?;
        let (name, secret) = store.cookie_key();
        let old = cookie_at(
            &name,
            &issued.token,
            &CookieOptions {
                secret: secret.clone(),
                secure: true,
                max_age: Some((SIGN_IN_IDLE_MS / 1000) as i64),
                same_site: SameSite::Strict,
            },
            now_seconds() - ROLL_MS / 1000 - 1,
        )?;
        for (provenance, secure) in [
            (Provenance::Local, false),
            (
                Provenance::Remote {
                    host: "example.org".into(),
                },
                true,
            ),
            (
                Provenance::Relayed {
                    relay: "relay".into(),
                },
                true,
            ),
        ] {
            let renewed = store
                .renew_cookie(&old, &name, secret.clone(), &provenance)?
                .ok_or("aging cookie was not renewed")?;
            assert_eq!(renewed.contains("; Secure"), secure);
        }
        Ok(())
    }

    #[test]
    fn revocation_closes_live_and_late_links_but_leaves_finished_links_alone()
    -> Result<(), SignInError> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (_root, store) = sign_ins();
        let issued = store.issue(&Principal::Owner, "device", SignInKind::Device, false)?;
        let calls = Arc::new(AtomicUsize::new(0));
        let closer = |weight| -> pi_rs_chord::node::http::LinkCloser {
            let calls = calls.clone();
            Arc::new(move || {
                calls.fetch_add(weight, Ordering::Relaxed);
            })
        };
        let _live = store.opened(issued.id.clone(), closer(1));
        if let Some(finished) = store.opened(issued.id.clone(), closer(10)) {
            finished();
        }
        assert!(store.revoke(&issued.id));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(store.opened(issued.id, closer(100)).is_none());
        assert_eq!(calls.load(Ordering::Relaxed), 101);
        Ok(())
    }

    #[test]
    fn an_issued_token_authenticates_as_its_principal_and_a_revoked_one_does_not() {
        let (_root, sign_ins) = sign_ins();
        let issued = sign_ins
            .issue(&Principal::Owner, "test", SignInKind::Device, false)
            .expect("issue");
        assert_eq!(sign_ins.verify(&issued.token), Some(Principal::Owner));
        assert!(issued.cookie.starts_with("p3-"));
        assert!(issued.cookie.contains("HttpOnly"));
        assert_eq!(sign_ins.list().len(), 1);
        assert!(sign_ins.revoke(&issued.id));
        assert_eq!(sign_ins.verify(&issued.token), None);
        assert!(!sign_ins.revoke(&issued.id));
    }

    #[test]
    fn a_token_that_is_not_the_issued_one_or_is_no_token_authenticates_nothing() {
        let (_root, sign_ins) = sign_ins();
        let issued = sign_ins
            .issue(&Principal::Owner, "test", SignInKind::Device, false)
            .expect("issue");
        let mut other = issued.token.clone();
        other.push('x');
        assert_eq!(sign_ins.verify(&other), None);
        for token in ["", ".", "abc", ".secret", "id.", "id.wrong"] {
            assert_eq!(sign_ins.verify(token), None, "{token}");
        }
    }

    #[test]
    fn sign_ins_are_kept_across_starts() {
        let root = tempfile::tempdir().expect("temp dir");
        let token = {
            let sign_ins = SignIns::open(root.path()).expect("open");
            sign_ins
                .issue(&Principal::Owner, "test", SignInKind::Browser, false)
                .expect("issue")
                .token
        };
        let reopened = SignIns::open(root.path()).expect("reopen");
        assert_eq!(reopened.verify(&token), Some(Principal::Owner));
    }

    #[test]
    fn the_owner_token_file_is_a_device_sign_in_of_the_owner() {
        let root = tempfile::tempdir().expect("temp dir");
        let sign_ins = SignIns::open(root.path()).expect("open");
        let path = owner_token_file(root.path());
        ensure_owner_token(&path, &sign_ins).expect("token file");
        let token = std::fs::read_to_string(&path)
            .expect("read")
            .trim()
            .to_string();
        assert_eq!(sign_ins.verify(&token), Some(Principal::Owner));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // A second call keeps the token that still authenticates.
        ensure_owner_token(&path, &sign_ins).expect("token file again");
        assert_eq!(std::fs::read_to_string(&path).expect("read").trim(), token);
    }

    #[test]
    fn an_owner_link_is_used_up_by_its_first_consumption() {
        let link = OwnerLink::default();
        let url = link.renew("http://127.0.0.1:8789");
        assert!(url.starts_with("http://127.0.0.1:8789/_p3/owner#"));
        let token = url.rsplit_once('#').expect("token").1;
        assert!(!link.consume("another"));
        assert!(link.consume(token));
        assert!(!link.consume(token));
    }

    #[test]
    fn a_hash_matches_only_its_token() {
        let hash = hash_token("secret");
        assert!(matches_hash("secret", &hash));
        assert!(!matches_hash("Secret", &hash));
        assert!(!matches_hash("secret", &hash[..hash.len() - 1]));
    }
}
