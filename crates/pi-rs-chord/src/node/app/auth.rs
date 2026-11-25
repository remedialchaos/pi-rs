use super::{new_token, secret_file, write_secret};
use crate::node::http::{
    CookieOptions, HttpContribution, HttpError, HttpRequest, HttpResponse, Provenance,
    RouteOptions, SameSite, cookie, json_response, now_seconds, read_cookie,
};
use crate::{
    Error, Result,
    context::Context,
    contracts::schema as s,
    documents::{Definition, DocumentStore, Documents, Handle},
    json::JsonValue,
    store::StoreEngine,
};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
};
pub const SIGN_IN_COOKIE: &str = "chord-sign-in";
const IDLE: u64 = 400 * 24 * 60 * 60;
fn hash(token: &str) -> String {
    crate::node::plugins::digest(token.as_bytes())
}
fn matches_hash(token: &str, stored: &str) -> bool {
    let given = hash(token);
    let Ok(a) = Hmac::<Sha256>::new_from_slice(given.as_bytes()) else {
        return false;
    };
    let Ok(b) = Hmac::<Sha256>::new_from_slice(stored.as_bytes()) else {
        return false;
    };
    b.verify_slice(&a.finalize().into_bytes()).is_ok()
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Session {
    hash: String,
    label: String,
    kind: String,
    principal: String,
    created: u64,
    last_used: u64,
}
struct Short {
    hash: String,
    principal: Value,
    created: u64,
    links: usize,
    expires: u64,
}
struct Pair {
    poll: String,
    label: String,
    address: String,
    expires: u64,
    token: Option<String>,
    approving: bool,
}
struct Invite {
    principal: Value,
    label: String,
    expires: u64,
}
struct Memory {
    short: BTreeMap<String, Short>,
    pairs: BTreeMap<String, Pair>,
    invites: BTreeMap<String, Invite>,
    owner: Option<(String, u64)>,
}
struct Inner {
    docs: Documents,
    handle: Handle,
    list: crate::state::Source,
    line: tokio::sync::Mutex<()>,
    memory: Mutex<Memory>,
    cookie: String,
    owner: Value,
}
#[derive(Clone)]
pub struct SignIns(Arc<Inner>);
impl SignIns {
    pub async fn open(store: StoreEngine, owner: Value, cookie_secret: String) -> Result<Self> {
        let list = crate::state::Source::new(store.batch(), JsonValue::from_serde(json!([])))?;
        let docs = Documents::new(
            "chord.app.sign-ins",
            store.batch(),
            Some(DocumentStore::from_engine(store, "chord.app.sign-ins")),
            |_| {},
        );
        let attempt = docs.attempt();
        let schema = s::obj([(
            "signIns",
            s::required(s::map(s::obj([
                ("hash", s::required(s::str())),
                ("label", s::required(s::str())),
                (
                    "kind",
                    s::required(s::one_of(vec![
                        JsonValue::from_serde(json!("browser")),
                        JsonValue::from_serde(json!("device")),
                    ])?),
                ),
                ("principal", s::required(s::str())),
                ("created", s::required(s::num())),
                ("lastUsed", s::required(s::num())),
            ]))),
        )]);
        let definition = Definition::new("chord.app.sign-ins", schema, 1, || {
            Ok(JsonValue::from_serde(json!({"signIns":{}})))
        })?
        .persisted(true, |_, _, n| n >= 50);
        let handle = attempt.declare(Arc::new(definition))?;
        attempt.activate(&Context::background()).await?;
        let sign_ins = Self(Arc::new(Inner {
            docs,
            handle,
            list,
            line: tokio::sync::Mutex::new(()),
            memory: Mutex::new(Memory {
                short: BTreeMap::new(),
                pairs: BTreeMap::new(),
                invites: BTreeMap::new(),
                owner: None,
            }),
            cookie: cookie_secret,
            owner,
        }));
        sign_ins.refresh_list()?;
        Ok(sign_ins)
    }
    fn sessions(&self) -> Result<BTreeMap<String, Session>> {
        let value = self.0.handle.value()?.to_json().to_serde()?;
        Ok(serde_json::from_value(
            value
                .get("signIns")
                .cloned()
                .ok_or_else(|| Error::Invalid("invalid sign-in document".into()))?,
        )?)
    }
    async fn save(&self, sessions: BTreeMap<String, Session>) -> Result<()> {
        let handle = self.0.handle.clone();
        self.0
            .docs
            .transaction(
                self.0.handle.token(),
                &Context::background(),
                move |tx| async move {
                    tx.draft(&handle, None).await?.set(
                        &[crate::delta::Segment::Key("signIns".into())],
                        JsonValue::from_serde(json!(sessions)),
                    )?;
                    Ok(())
                },
            )
            .await?;
        self.refresh_list()
    }
    fn refresh_list(&self) -> Result<()> {
        let mut list = Vec::new();
        for (id, session) in self.sessions()? {
            list.push(json!({"id":id,"label":session.label,"kind":session.kind,"principal":serde_json::from_str::<Value>(&session.principal)?,"created":session.created,"lastUsed":session.last_used}));
        }
        self.0
            .list
            .set(JsonValue::from_serde(json!(list)), &Context::background())
    }
    pub fn facet(&self) -> Result<crate::host::Facet> {
        use crate::{
            contracts::{Contract, Member},
            host::{Members, ServeOptions, define_facet},
        };
        use indexmap::IndexMap;
        let contract = Arc::new(Contract::local(
            "chord.app.sign-ins",
            1,
            vec![],
            IndexMap::from([
                (
                    "list".into(),
                    Member::State(s::arr(s::obj([
                        ("id", s::required(s::str())),
                        ("label", s::required(s::str())),
                        (
                            "kind",
                            s::required(s::one_of(vec![
                                JsonValue::from_serde(json!("browser")),
                                JsonValue::from_serde(json!("device")),
                            ])?),
                        ),
                        ("principal", s::required(s::unknown())),
                        ("created", s::required(s::num())),
                        ("lastUsed", s::required(s::num())),
                    ]))),
                ),
                (
                    "issue".into(),
                    Member::Method {
                        args: s::obj([
                            ("principal", s::required(s::unknown())),
                            ("label", s::required(s::str())),
                            (
                                "kind",
                                s::required(s::one_of(vec![
                                    JsonValue::from_serde(json!("browser")),
                                    JsonValue::from_serde(json!("device")),
                                    JsonValue::from_serde(json!("ephemeral")),
                                ])?),
                            ),
                            ("secure", s::opt(s::bool())),
                        ]),
                        result: s::obj([
                            ("id", s::required(s::str())),
                            ("token", s::required(s::str())),
                            ("cookie", s::required(s::str())),
                        ]),
                    },
                ),
                (
                    "verify".into(),
                    Member::Method {
                        args: s::str(),
                        result: s::unknown(),
                    },
                ),
                (
                    "revoke".into(),
                    Member::Method {
                        args: s::str(),
                        result: s::nul(),
                    },
                ),
            ]),
        )?);
        let sign_ins = self.clone();
        Ok(define_facet(move |host| {
            let sign_ins = sign_ins.clone();
            host.serve(
                contract.clone(),
                move |_, _, self_| {
                    let mut members = Members::new();
                    members.state("list", self_.follow_source(sign_ins.0.list.clone())?)?;
                    for method in ["issue", "verify", "revoke"] {
                        let sign_ins = sign_ins.clone();
                        members.method(method, move |args, _| {
                            let sign_ins = sign_ins.clone();
                            Box::pin(async move {
                                let args = args.to_serde()?;
                                let value = match method {
                                    "issue" => {
                                        let label = args["label"].as_str().ok_or_else(|| {
                                            Error::Invalid("missing label".into())
                                        })?;
                                        let kind = args["kind"]
                                            .as_str()
                                            .ok_or_else(|| Error::Invalid("missing kind".into()))?;
                                        serde_json::to_value(
                                            sign_ins
                                                .issue(
                                                    args["principal"].clone(),
                                                    label,
                                                    kind,
                                                    args["secure"].as_bool().unwrap_or(false),
                                                )
                                                .await?,
                                        )?
                                    }
                                    "verify" => sign_ins
                                        .verify(args.as_str().ok_or_else(|| {
                                            Error::Invalid("token must be text".into())
                                        })?)
                                        .await?
                                        .unwrap_or(Value::Null),
                                    _ => {
                                        sign_ins
                                            .revoke(args.as_str().ok_or_else(|| {
                                                Error::Invalid("id must be text".into())
                                            })?)
                                            .await?;
                                        Value::Null
                                    }
                                };
                                Ok(JsonValue::from_serde(value))
                            })
                        })?;
                    }
                    Ok(members)
                },
                ServeOptions::default(),
            )
        }))
    }
    pub async fn issue(
        &self,
        principal: Value,
        label: &str,
        kind: &str,
        secure: bool,
    ) -> Result<IssuedSignIn> {
        if !matches!(kind, "browser" | "device" | "ephemeral") {
            return Err(Error::Invalid("invalid sign-in kind".into()));
        }
        let id = new_token(12);
        let secret = new_token(32);
        let token = format!("{id}.{secret}");
        let now = now_seconds();
        if kind == "ephemeral" {
            let mut memory = self.0.memory.lock().unwrap_or_else(|e| e.into_inner());
            if memory.short.len() >= 1000
                && let Some(id) = memory
                    .short
                    .iter()
                    .min_by_key(|(_, v)| v.created)
                    .map(|(id, _)| id.clone())
            {
                memory.short.remove(&id);
            }
            memory.short.insert(
                id.clone(),
                Short {
                    hash: hash(&secret),
                    principal,
                    created: now,
                    links: 0,
                    expires: now + 60,
                },
            );
        } else {
            let _line = self.0.line.lock().await;
            let mut sessions = self.sessions()?;
            sessions.insert(
                id.clone(),
                Session {
                    hash: hash(&secret),
                    label: label.into(),
                    kind: kind.into(),
                    principal: serde_json::to_string(&principal)?,
                    created: now * 1000,
                    last_used: now * 1000,
                },
            );
            self.save(sessions).await?;
        }
        let options = CookieOptions {
            secret: self.0.cookie.clone(),
            secure,
            max_age: (kind != "ephemeral").then_some(IDLE as i64),
            same_site: SameSite::Strict,
        };
        Ok(IssuedSignIn {
            id,
            token: token.clone(),
            cookie: cookie(SIGN_IN_COOKIE, &token, &options)
                .map_err(|e| Error::Invalid(e.to_string()))?,
        })
    }
    pub async fn verify(&self, token: &str) -> Result<Option<Value>> {
        let Some((id, secret)) = token.split_once('.') else {
            return Ok(None);
        };
        {
            let mut memory = self.0.memory.lock().unwrap_or_else(|e| e.into_inner());
            memory
                .short
                .retain(|_, s| s.links > 0 || s.expires > now_seconds());
            if let Some(short) = memory.short.get(id) {
                return Ok(matches_hash(secret, &short.hash).then(|| short.principal.clone()));
            }
        }
        let _line = self.0.line.lock().await;
        let mut sessions = self.sessions()?;
        let Some(session) = sessions.get_mut(id) else {
            return Ok(None);
        };
        if !matches_hash(secret, &session.hash) {
            return Ok(None);
        }
        let now = now_seconds() * 1000;
        if now.saturating_sub(session.last_used) > IDLE * 1000 {
            sessions.remove(id);
            self.save(sessions).await?;
            return Ok(None);
        }
        let principal = serde_json::from_str(&session.principal)?;
        if now.saturating_sub(session.last_used) > 60_000 {
            session.last_used = now;
            self.save(sessions).await?;
        }
        Ok(Some(principal))
    }
    pub async fn revoke(&self, id: &str) -> Result<()> {
        self.0
            .memory
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .short
            .remove(id);
        let _line = self.0.line.lock().await;
        let mut sessions = self.sessions()?;
        sessions.remove(id);
        self.save(sessions).await
    }
    pub fn end_ephemeral(&self, id: &str) {
        self.0
            .memory
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .short
            .remove(id);
    }
    pub fn opened(&self, request: &HttpRequest) -> Option<Box<dyn FnOnce() + Send>> {
        let token = request
            .header("authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::to_string)
            .or_else(|| {
                read_cookie(
                    request.header("cookie").unwrap_or(""),
                    SIGN_IN_COOKIE,
                    &self.0.cookie,
                )
            })?;
        let (id, secret) = token.split_once('.')?;
        {
            let mut memory = self.0.memory.lock().unwrap_or_else(|e| e.into_inner());
            let short = memory.short.get_mut(id)?;
            if !matches_hash(secret, &short.hash)
                || short.links == 0 && short.expires <= now_seconds()
            {
                return None;
            }
            short.links += 1;
        }
        let sign_ins = self.clone();
        let id = id.to_string();
        Some(Box::new(move || {
            if let Some(short) = sign_ins
                .0
                .memory
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .short
                .get_mut(&id)
            {
                short.links = short.links.saturating_sub(1);
                if short.links == 0 {
                    short.expires = now_seconds() + 60;
                }
            }
        }))
    }
    pub async fn owner_token_file(&self, path: &Path) -> Result<String> {
        if let Ok(existing) = std::fs::read_to_string(path) {
            let existing = existing.trim();
            if self.verify(existing).await?.as_ref() == Some(&self.0.owner) {
                return Ok(existing.into());
            }
        }
        let issued = self
            .issue(self.0.owner.clone(), "Owner token file", "device", false)
            .await?;
        write_secret(path, &issued.token, true)?;
        Ok(issued.token)
    }
    pub fn owner_link(&self) -> String {
        let token = new_token(32);
        self.0
            .memory
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .owner = Some((hash(&token), now_seconds() + 600));
        format!("/_chord/owner#{token}")
    }
    fn owner(&self, request: &HttpRequest) -> std::result::Result<(), HttpError> {
        if request.principal.as_ref() == Some(&self.0.owner) {
            Ok(())
        } else {
            Err(HttpError::new(403, "owner required"))
        }
    }
    pub async fn authenticate(&self, request: &HttpRequest) -> Result<Option<Value>> {
        let bearer = request
            .header("authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::to_string);
        let token = bearer.or_else(|| {
            read_cookie(
                request.header("cookie").unwrap_or(""),
                SIGN_IN_COOKIE,
                &self.0.cookie,
            )
        });
        match token {
            Some(token) => self.verify(&token).await,
            None => Ok(None),
        }
    }
    pub fn contribution(&self) -> std::result::Result<HttpContribution, HttpError> {
        let mut contribution = HttpContribution::new("chord.app.auth");
        let signins = self.clone();
        contribution.authenticators.push(Arc::new(move |request| {
            let signins = signins.clone();
            Box::pin(async move { Ok(signins.authenticate(&request).await?) })
        }));
        for (pattern, public) in [
            ("POST /_chord/owner", true),
            ("GET /_chord/owner", true),
            ("POST /_chord/pair", true),
            ("POST /_chord/pair/poll", true),
            ("GET /_chord/pair/:code", false),
            ("POST /_chord/pair/:code", false),
            ("POST /_chord/invites", false),
            ("GET /_chord/invite", true),
            ("POST /_chord/invite", true),
            ("POST /_chord/login", true),
        ] {
            let signins = self.clone();
            let mut route = RouteOptions::new(move |request| {
                let signins = signins.clone();
                Box::pin(async move { signins.route(request).await })
            });
            route.public = public;
            route.page = pattern.starts_with("GET");
            contribution.route(pattern, route)?;
        }
        Ok(contribution)
    }
    async fn route(&self, request: HttpRequest) -> std::result::Result<HttpResponse, HttpError> {
        let secure = !matches!(request.provenance, Provenance::Local);
        if request.method == ::http::Method::GET {
            if request.path.starts_with("/_chord/pair/") {
                self.owner(&request)?;
                return super::super::http::html_response(
                    "<form method=post><button>Approve device</button></form>",
                );
            }
            let script = "const token=location.hash.slice(1);history.replaceState(null,'',location.pathname);fetch(location.pathname,{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({token})}).then(r=>r.ok?location.replace('/'):document.body.textContent='Sign-in refused');";
            return super::super::http::html_response(format!(
                "<!doctype html><body>Signing in<script nonce=\"{}\">{script}</script>",
                request.nonce
            ));
        }
        let bytes = request.body(16 * 1024).await?;
        let body: Value = if bytes.is_empty() {
            json!({})
        } else {
            serde_json::from_slice(&bytes)?
        };
        let required = |name: &str| {
            body.get(name)
                .and_then(Value::as_str)
                .ok_or_else(|| HttpError::new(400, format!("missing {name}")))
        };
        match request.path.as_str() {
            "/_chord/owner" => {
                let token = required("token")?;
                let valid = {
                    let mut memory = self.0.memory.lock().unwrap_or_else(|e| e.into_inner());
                    let valid = memory
                        .owner
                        .as_ref()
                        .is_some_and(|(kept, _)| matches_hash(token, kept));
                    if valid {
                        memory
                            .owner
                            .take()
                            .is_some_and(|(_, expires)| expires > now_seconds())
                    } else {
                        false
                    }
                };
                if !valid {
                    return Err(HttpError::new(403, "owner link refused"));
                }
                issued_response(
                    self.issue(self.0.owner.clone(), "Owner browser", "browser", secure)
                        .await?,
                    json!("signed in"),
                )
            }
            "/_chord/pair" => {
                let mut memory = self.0.memory.lock().unwrap_or_else(|e| e.into_inner());
                let now = now_seconds();
                memory.pairs.retain(|_, pair| pair.expires > now);
                let address = request
                    .remote_address
                    .map(|a| a.ip().to_string())
                    .unwrap_or_default();
                if request.principal.as_ref() != Some(&self.0.owner)
                    && (memory.pairs.len() >= 100
                        || memory
                            .pairs
                            .values()
                            .filter(|p| p.address == address)
                            .count()
                            >= 10)
                {
                    return Err(HttpError::new(429, "too many pairings"));
                }
                let code = loop {
                    let alphabet = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
                    let code = (0..8)
                        .map(|_| alphabet[rand::random::<u8>() as usize % alphabet.len()] as char)
                        .collect::<String>();
                    if !memory.pairs.contains_key(&code) {
                        break code;
                    }
                };
                let poll = new_token(32);
                memory.pairs.insert(
                    code.clone(),
                    Pair {
                        poll: hash(&poll),
                        label: body
                            .get("label")
                            .and_then(Value::as_str)
                            .unwrap_or("Device")
                            .chars()
                            .take(120)
                            .collect(),
                        address,
                        expires: now + 600,
                        token: None,
                        approving: false,
                    },
                );
                json_response(&json!({"code":code,"poll":poll}))
            }
            "/_chord/pair/poll" => {
                let poll = required("poll")?;
                let mut memory = self.0.memory.lock().unwrap_or_else(|e| e.into_inner());
                let code = memory
                    .pairs
                    .iter()
                    .find(|(_, p)| p.expires > now_seconds() && matches_hash(poll, &p.poll))
                    .map(|(code, _)| code.clone())
                    .ok_or_else(|| HttpError::new(404, "pairing ended"))?;
                if let Some(token) = memory.pairs.get(&code).and_then(|p| p.token.clone()) {
                    memory.pairs.remove(&code);
                    json_response(&json!({"status":"approved","token":token}))
                } else {
                    json_response(&json!({"status":"waiting"}))
                }
            }
            "/_chord/invites" => {
                self.owner(&request)?;
                let principal = body
                    .get("principal")
                    .filter(|v| v.is_object())
                    .cloned()
                    .ok_or_else(|| HttpError::new(400, "principal must be an object"))?;
                let label = required("label")?;
                let token = new_token(32);
                self.0
                    .memory
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .invites
                    .insert(
                        hash(&token),
                        Invite {
                            principal,
                            label: label.into(),
                            expires: now_seconds() + 7 * 24 * 3600,
                        },
                    );
                json_response(&json!({"path":format!("/_chord/invite#{token}")}))
            }
            "/_chord/invite" => {
                let token = required("token")?;
                let invite = self
                    .0
                    .memory
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .invites
                    .remove(&hash(token))
                    .filter(|v| v.expires > now_seconds())
                    .ok_or_else(|| HttpError::new(403, "invite ended"))?;
                issued_response(
                    self.issue(invite.principal, &invite.label, "browser", secure)
                        .await?,
                    json!("signed in"),
                )
            }
            "/_chord/login" => {
                let name = required("name")?.trim();
                if name.is_empty() || name.encode_utf16().count() > 40 {
                    return Err(HttpError::new(400, "invalid login name"));
                }
                let principal = json!({"kind":"user","id":new_token(12),"name":name});
                issued_response(
                    self.issue(principal.clone(), name, "ephemeral", secure)
                        .await?,
                    principal,
                )
            }
            _ if request.path.starts_with("/_chord/pair/") => {
                self.owner(&request)?;
                let code = request
                    .params
                    .get("code")
                    .ok_or_else(|| HttpError::new(404, "pairing ended"))?;
                let label = {
                    let mut memory = self.0.memory.lock().unwrap_or_else(|e| e.into_inner());
                    let pair = memory
                        .pairs
                        .get_mut(code)
                        .filter(|p| p.expires > now_seconds() && p.token.is_none() && !p.approving)
                        .ok_or_else(|| HttpError::new(404, "pairing ended"))?;
                    pair.approving = true;
                    pair.label.clone()
                };
                let issued = self
                    .issue(self.0.owner.clone(), &label, "device", secure)
                    .await;
                if let Some(pair) = self
                    .0
                    .memory
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .pairs
                    .get_mut(code)
                {
                    pair.approving = false;
                    if let Ok(issued) = &issued {
                        pair.token = Some(issued.token.clone());
                    }
                }
                issued?;
                json_response(&json!("approved"))
            }
            _ => Err(HttpError::new(404, "not found")),
        }
    }
    pub fn close(&self) {
        let mut memory = self.0.memory.lock().unwrap_or_else(|e| e.into_inner());
        memory.short.clear();
        memory.pairs.clear();
        memory.invites.clear();
        memory.owner = None;
        drop(memory);
        self.0.docs.end();
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct IssuedSignIn {
    pub id: String,
    pub token: String,
    pub cookie: String,
}
fn issued_response(
    issued: IssuedSignIn,
    value: Value,
) -> std::result::Result<HttpResponse, HttpError> {
    let mut response = json_response(&value)?;
    response
        .headers_mut()
        .insert("Set-Cookie", ::http::HeaderValue::from_str(&issued.cookie)?);
    Ok(response)
}
pub fn cookie_secret(path: &Path) -> Result<String> {
    secret_file(path)
}
