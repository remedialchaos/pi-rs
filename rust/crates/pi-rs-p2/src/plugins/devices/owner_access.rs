//! Installation-owned signing key, single-use association tokens, local challenges, and
//! HMAC-signed owner session cookies matching `apps/p2/plugins/devices/owner-access.ts`.

use std::{
    collections::BTreeMap,
    fmt::Write,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use hmac::{Hmac, Mac};
use indexmap::IndexMap;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::runtime::local_owner::local_owner_proof;

pub const TOKEN_LIFETIME_MS: u64 = 10 * 60_000;
pub const CHALLENGE_LIFETIME_MS: u64 = 60_000;
pub const COOKIE_LIFETIME_MS: u64 = 30 * 24 * 60 * 60_000;
const MAX_PENDING: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnerSessionKind {
    Device,
    Local,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnerSession {
    pub kind: OwnerSessionKind,
    pub id: String,
    pub expires: u64,
}

pub fn owner_cookie_name(port: u16) -> String {
    format!("p2_owner_{port}")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn random_hex_16() -> String {
    hex_encode(Uuid::new_v4().as_bytes())
}

fn random_hex_32() -> String {
    let mut out = String::with_capacity(64);
    out.push_str(&random_hex_16());
    out.push_str(&random_hex_16());
    out
}

fn sha256_hex(value: &str) -> String {
    hex_encode(&Sha256::digest(value.as_bytes()))
}

fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

fn private_write(path: &Path, content: &str) -> std::io::Result<()> {
    let suffix = &random_hex_16()[..16];
    let temp = PathBuf::from(format!("{}.{suffix}.tmp", path.display()));
    let write_res = (|| -> std::io::Result<()> {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&temp)?;
        std::io::Write::write_all(&mut file, content.as_bytes())?;
        drop(file);
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if write_res.is_err() {
        let _ = fs::remove_file(&temp);
    }
    write_res
}

pub fn cookie_value<'a>(cookie_header: &'a str, name: &str) -> Option<&'a str> {
    cookie_header.split(';').find_map(|pair| {
        let (key, value) = pair.trim().split_once('=')?;
        (key == name).then_some(value.trim())
    })
}

type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

struct Inner {
    directory: PathBuf,
    key: String,
    invitations: IndexMap<String, u64>,
    challenges: IndexMap<String, u64>,
    revoked: BTreeMap<String, u64>,
    now: Clock,
}

impl Inner {
    fn issue(pending: &mut IndexMap<String, u64>, now: u64, lifetime: u64) -> String {
        pending.retain(|_, expires| *expires > now);
        if pending.len() >= MAX_PENDING {
            pending.shift_remove_index(0);
        }
        let token = random_hex_32();
        pending.insert(sha256_hex(&token), now.saturating_add(lifetime));
        token
    }

    fn redeem(pending: &mut IndexMap<String, u64>, now: u64, token: &str) -> bool {
        let id = sha256_hex(token);
        let expires = pending.shift_remove(&id);
        expires.is_some_and(|exp| exp > now)
    }

    fn signature(&self, port: u16, payload: &str) -> String {
        // INFALLIBLE: HMAC-SHA256 accepts keys of any length.
        let mut mac =
            Hmac::<Sha256>::new_from_slice(self.key.as_bytes()).expect("HMAC accepts any key");
        mac.update(format!("{port}:{payload}").as_bytes());
        hex_encode(&mac.finalize().into_bytes())
    }

    fn session_cookie(&self, port: u16, prefix: &str, id: &str, secure: bool) -> String {
        let expires = (self.now)().saturating_add(COOKIE_LIFETIME_MS);
        let payload = format!("{prefix}{id}.{expires}");
        let sig = self.signature(port, &payload);
        let secure_attr = if secure { "; Secure" } else { "" };
        format!(
            "{}={payload}.{sig}; HttpOnly; SameSite=Lax; Path=/; Max-Age=2592000{secure_attr}",
            owner_cookie_name(port)
        )
    }
}

#[derive(Clone)]
pub struct OwnerAccess(Arc<Mutex<Inner>>);

impl OwnerAccess {
    pub fn open(directory: impl Into<PathBuf>) -> std::io::Result<Self> {
        Self::open_with_clock(directory, Arc::new(now_ms))
    }

    pub fn open_with_clock(directory: impl Into<PathBuf>, now: Clock) -> std::io::Result<Self> {
        let directory = directory.into();
        fs::create_dir_all(&directory)?;
        set_mode(&directory, 0o700)?;

        let key_path = directory.join("owner-key");
        let mut key = match fs::read_to_string(&key_path) {
            Ok(raw) => raw.trim().to_owned(),
            Err(err) if err.kind() == ErrorKind::NotFound => {
                let generated = random_hex_32();
                private_write(&key_path, &generated)?;
                generated
            }
            Err(err) => return Err(err),
        };
        if !is_lower_hex(&key, 64) {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                "Invalid owner key",
            ));
        }
        set_mode(&key_path, 0o600)?;

        let scheme_path = directory.join("owner-cookie-version");
        let scheme = match fs::read_to_string(&scheme_path) {
            Ok(raw) => Some(raw.trim().to_owned()),
            Err(err) if err.kind() == ErrorKind::NotFound => None,
            Err(err) => return Err(err),
        };
        match scheme.as_deref() {
            None => {
                key = random_hex_32();
                private_write(&key_path, &key)?;
                private_write(&scheme_path, "signed-v1\n")?;
            }
            Some("signed-v1") => {}
            Some(_) => {
                return Err(std::io::Error::new(
                    ErrorKind::InvalidData,
                    "Invalid owner cookie version",
                ));
            }
        }

        let revoked_path = directory.join("owner-revoked.json");
        let revoked: BTreeMap<String, u64> = match fs::read_to_string(&revoked_path) {
            Ok(raw) => {
                let parsed: BTreeMap<String, u64> = serde_json::from_str(&raw).map_err(|_| {
                    std::io::Error::new(ErrorKind::InvalidData, "Cannot read owner revocations")
                })?;
                if parsed.keys().any(|id| !is_lower_hex(id, 32)) {
                    return Err(std::io::Error::new(
                        ErrorKind::InvalidData,
                        "Invalid owner revocations",
                    ));
                }
                parsed
            }
            Err(err) if err.kind() == ErrorKind::NotFound => BTreeMap::new(),
            Err(_) => {
                return Err(std::io::Error::new(
                    ErrorKind::InvalidData,
                    "Cannot read owner revocations",
                ));
            }
        };

        let mut inner = Inner {
            directory: directory.clone(),
            key,
            invitations: IndexMap::new(),
            challenges: IndexMap::new(),
            revoked,
            now,
        };
        let startup_token = Inner::issue(&mut inner.invitations, (inner.now)(), TOKEN_LIFETIME_MS);
        private_write(
            &directory.join("owner-token.txt"),
            &format!("{startup_token}\n"),
        )?;
        Ok(Self(Arc::new(Mutex::new(inner))))
    }

    pub fn issue_token(&self) -> String {
        let mut inner = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        let now = (inner.now)();
        Inner::issue(&mut inner.invitations, now, TOKEN_LIFETIME_MS)
    }

    pub fn claim(&self, token: &str) -> bool {
        let mut inner = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        let now = (inner.now)();
        Inner::redeem(&mut inner.invitations, now, token.trim())
    }

    pub fn issue_challenge(&self) -> String {
        let mut inner = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        let now = (inner.now)();
        Inner::issue(&mut inner.challenges, now, CHALLENGE_LIFETIME_MS)
    }

    pub fn prove_local(&self, challenge: &str, proof: &str, port: u16) -> bool {
        if !is_lower_hex(proof, 64) {
            return false;
        }
        let mut inner = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        let now = (inner.now)();
        if !Inner::redeem(&mut inner.challenges, now, challenge) {
            return false;
        }
        let expected = local_owner_proof(&inner.key, port, challenge);
        constant_time_eq(proof, &expected)
    }

    pub fn cookie(&self, port: u16, secure: bool) -> String {
        let inner = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        inner.session_cookie(port, "", &random_hex_16(), secure)
    }

    pub fn local_cookie(&self, port: u16, id: Option<&str>) -> String {
        let inner = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        let owned;
        let session_id = match id {
            Some(existing) => existing,
            None => {
                owned = random_hex_16();
                &owned
            }
        };
        inner.session_cookie(port, "local.", session_id, false)
    }

    pub fn clear_cookie(&self, port: u16, secure: bool) -> String {
        let secure_attr = if secure { "; Secure" } else { "" };
        format!(
            "{}=; HttpOnly{secure_attr}; SameSite=Lax; Path=/; Max-Age=0",
            owner_cookie_name(port)
        )
    }

    pub fn session_from_cookie_header(
        &self,
        cookie_header: &str,
        port: u16,
    ) -> Option<OwnerSession> {
        let value = cookie_value(cookie_header, &owner_cookie_name(port))?;
        let (prefix, rest) = match value.strip_prefix("local.") {
            Some(stripped) => ("local.", stripped),
            None => ("", value),
        };
        let mut parts = rest.split('.');
        let id = parts.next()?;
        let expiry = parts.next()?;
        let signature = parts.next()?;
        if parts.next().is_some()
            || !is_lower_hex(id, 32)
            || !is_lower_hex(signature, 64)
            || expiry.is_empty()
            || !expiry.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let expires: u64 = expiry.parse().ok()?;
        let inner = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        let now = (inner.now)();
        if expires <= now || inner.revoked.get(id).copied().unwrap_or(0) > now {
            return None;
        }
        let payload = format!("{prefix}{id}.{expiry}");
        let expected = inner.signature(port, &payload);
        if !constant_time_eq(signature, &expected) {
            return None;
        }
        Some(OwnerSession {
            kind: if prefix.is_empty() {
                OwnerSessionKind::Device
            } else {
                OwnerSessionKind::Local
            },
            id: id.into(),
            expires,
        })
    }

    pub fn logout(&self, session: &OwnerSession) -> std::io::Result<()> {
        let mut inner = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        let now = (inner.now)();
        inner.revoked.retain(|_, expires| *expires > now);
        inner.revoked.insert(session.id.clone(), session.expires);
        let serialized = serde_json::to_string(&inner.revoked)
            .map_err(|err| std::io::Error::new(ErrorKind::InvalidData, err))?;
        private_write(&inner.directory.join("owner-revoked.json"), &serialized)
    }

    pub fn rotate(&self) -> std::io::Result<()> {
        let mut inner = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        let next = random_hex_32();
        private_write(&inner.directory.join("owner-key"), &next)?;
        inner.key = next;
        inner.invitations.clear();
        inner.challenges.clear();
        inner.revoked.clear();
        private_write(&inner.directory.join("owner-revoked.json"), "{}")?;
        let now = (inner.now)();
        let token = Inner::issue(&mut inner.invitations, now, TOKEN_LIFETIME_MS);
        private_write(
            &inner.directory.join("owner-token.txt"),
            &format!("{token}\n"),
        )
    }

    pub fn close(&self) {
        let mut inner = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        inner.invitations.clear();
        inner.challenges.clear();
    }
}
