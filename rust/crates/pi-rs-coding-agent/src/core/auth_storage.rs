//! Port of `packages/coding-agent/src/core/auth-storage.ts`.
//!
//! `auth.json` maps provider IDs to `pi_rs_ai` credentials. All mutations run
//! under the crate's directory lock, read the file fresh before writing, and
//! never overwrite a file that cannot be parsed. New files are created with
//! owner-only permissions on Unix.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use pi_rs_ai::auth::{Credential, CredentialInfo};

pub use super::resolve_config_value::is_command_config_value;
use super::resolve_config_value::resolve_config_value_str_with_env;
use crate::lockfile::{LockOptions, lock_sync};
use crate::utils::text::strip_bom;

/// Result of a locked read-modify-write callback, matching upstream's
/// `{ result, next }` shape. `next` of `None` leaves the file untouched.
#[derive(Debug)]
pub struct LockResult<T> {
    pub result: T,
    pub next: Option<String>,
}

/// Credential storage backed by a JSON file.
pub struct AuthStorage {
    path: PathBuf,
    data: IndexMap<String, Credential>,
}

impl AuthStorage {
    /// Create a store for `path`, loading existing credentials. A missing file
    /// is created with mode `0o600` (Unix) and its parent with mode `0o700`.
    pub fn create(path: PathBuf) -> Self {
        let mut storage = Self {
            path,
            data: IndexMap::new(),
        };
        // A malformed file must not prevent construction; the last valid
        // in-memory snapshot stays empty until a successful reload.
        let _ = storage.reload();
        storage
    }

    /// Create a store from a path string, expanding a leading `~`.
    pub fn create_from_str(path: &str) -> Self {
        Self::create(PathBuf::from(crate::config::normalize_path(path, false)))
    }

    /// Create a store at the default `~/.pi/agent/auth.json` path.
    pub fn create_default() -> Result<Self, String> {
        Ok(Self::create(crate::config::get_auth_path()?))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reload credentials from disk. On a parse error the cached data is kept.
    pub fn reload(&mut self) -> Result<(), String> {
        let loaded = self.with_lock(|current| {
            let data = parse_data(current)?;
            Ok(LockResult {
                result: data.clone(),
                next: None,
            })
        })?;
        self.data = loaded;
        Ok(())
    }

    /// Cached credentials, without resolving configured values.
    pub fn data(&self) -> &IndexMap<String, Credential> {
        &self.data
    }

    /// Get a credential, resolving `api_key` keys that reference environment
    /// variables or `!command` values. OAuth credentials are returned as-is.
    pub fn get(&self, provider: &str) -> Option<Credential> {
        let credential = self.data.get(provider)?.clone();
        let mut api_key = match credential {
            Credential::ApiKey(api_key) => api_key,
            other => return Some(other),
        };
        let Some(raw) = api_key.key.clone() else {
            return Some(Credential::ApiKey(api_key));
        };
        let env: HashMap<String, String> = api_key
            .env
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        api_key.key = resolve_config_value_str_with_env(&raw, provider, "apiKey", Some(&env));
        Some(Credential::ApiKey(api_key))
    }

    /// List provider IDs and credential types in file insertion order.
    pub fn list(&self) -> Vec<CredentialInfo> {
        self.data
            .iter()
            .map(|(provider_id, credential)| CredentialInfo {
                provider_id: provider_id.clone(),
                auth_type: credential.auth_type(),
            })
            .collect()
    }

    /// Store a credential. Returns the previous value, if any.
    pub fn set(
        &mut self,
        provider: &str,
        credential: Credential,
    ) -> Result<Option<Credential>, String> {
        self.with_data_lock(|data| {
            let previous = data.get(provider).cloned();
            let mut next = data.clone();
            next.insert(provider.to_string(), credential);
            Ok((previous, Some(next)))
        })
    }

    /// Remove a credential. Returns the removed value, if any.
    pub fn delete(&mut self, provider: &str) -> Result<Option<Credential>, String> {
        self.with_data_lock(|data| {
            let mut next = data.clone();
            let removed = next.shift_remove(provider);
            if removed.is_some() {
                Ok((removed, Some(next)))
            } else {
                Ok((removed, None))
            }
        })
    }

    /// Run a raw read-modify-write callback under the file lock. The callback
    /// receives the file text (`None` when missing); returning `next` commits it.
    pub fn with_lock<T>(
        &mut self,
        callback: impl FnOnce(Option<&str>) -> Result<LockResult<T>, String>,
    ) -> Result<T, String> {
        ensure_parent_dir(&self.path)?;
        ensure_file_exists(&self.path)?;

        let _guard = lock_sync(
            &self.path,
            &LockOptions {
                realpath: true,
                retries: 10,
                ..LockOptions::default()
            },
        )?;
        let current = match std::fs::read_to_string(&self.path) {
            Ok(content) => Some(content),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.to_string()),
        };
        let LockResult { result, next } = callback(current.as_deref())?;
        if let Some(next) = next {
            write_file(&self.path, &next)?;
            self.data = parse_data(Some(&next))?;
        } else if let Some(content) = current {
            // The file may have been changed by another process; keep our
            // snapshot in sync with what we just read under the lock.
            if let Ok(data) = parse_data(Some(&content)) {
                self.data = data;
            }
        }
        Ok(result)
    }

    /// Like [`AuthStorage::with_lock`], but operating on parsed credentials.
    pub fn with_data_lock<T>(
        &mut self,
        callback: impl FnOnce(
            &IndexMap<String, Credential>,
        ) -> Result<(T, Option<IndexMap<String, Credential>>), String>,
    ) -> Result<T, String> {
        self.with_lock(|current| {
            let data = parse_data(current)?;
            let (result, next) = callback(&data)?;
            let next = match next {
                Some(entries) => Some(
                    serde_json::to_string_pretty(&entries).map_err(|error| error.to_string())?,
                ),
                None => None,
            };
            Ok(LockResult { result, next })
        })
    }
}

/// One-off read of a stored credential without creating or locking the file.
pub fn read_stored_credential(provider: &str, path: &Path) -> Option<Credential> {
    let content = std::fs::read_to_string(path).ok()?;
    let data = parse_data(Some(&content)).ok()?;
    data.get(provider).cloned()
}

fn parse_data(content: Option<&str>) -> Result<IndexMap<String, Credential>, String> {
    let Some(content) = content else {
        return Ok(IndexMap::new());
    };
    serde_json::from_str(strip_bom(content)).map_err(|error| format!("Invalid auth.json: {error}"))
}

pub(super) fn ensure_parent_dir(path: &Path) -> Result<(), String> {
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(parent).map_err(|error| error.to_string())
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())
    }
}

pub(super) fn ensure_file_exists(path: &Path) -> Result<(), String> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(mut file) => file.write_all(b"{}").map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn write_file(path: &Path, content: &str) -> Result<(), String> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).truncate(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|error| error.to_string())?;
    file.write_all(content.as_bytes())
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn root() -> Result<tempfile::TempDir, String> {
        tempfile::tempdir().map_err(|error| error.to_string())
    }

    fn write(path: &Path, content: &str) -> Result<(), String> {
        std::fs::write(path, content).map_err(|error| error.to_string())
    }

    fn api_key(key: &str) -> Credential {
        Credential::ApiKey(pi_rs_ai::auth::ApiKeyCredential {
            key: Some(key.to_string()),
            env: IndexMap::new(),
        })
    }

    #[test]
    fn create_writes_owner_only_file_and_directory() -> Result<(), String> {
        let temp = root()?;
        let path = temp.path().join("agent").join("auth.json");
        let storage = AuthStorage::create(path.clone());
        assert!(storage.get("openai").is_none());
        assert_eq!(
            std::fs::read_to_string(&path).map_err(|e| e.to_string())?,
            "{}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path)
                    .map_err(|e| e.to_string())?
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(path.parent().ok_or("missing parent")?)
                    .map_err(|e| e.to_string())?
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        Ok(())
    }

    #[test]
    fn set_get_delete_and_list_roundtrip() -> Result<(), String> {
        let temp = root()?;
        let path = temp.path().join("auth.json");
        let mut storage = AuthStorage::create(path.clone());
        assert!(storage.set("openai", api_key("key-1"))?.is_none());
        assert!(storage.set("anthropic", api_key("key-2"))?.is_none());
        assert!(storage.get("openai") == Some(api_key("key-1")));
        assert_eq!(storage.list().len(), 2);
        assert_eq!(storage.list()[0].provider_id, "openai");
        assert!(storage.delete("openai")? == Some(api_key("key-1")));
        assert!(storage.get("openai").is_none());
        assert_eq!(storage.list().len(), 1);
        let saved = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        assert!(!saved.contains("openai"));
        Ok(())
    }

    #[test]
    fn resolves_environment_references_from_credential_env() -> Result<(), String> {
        let temp = root()?;
        let path = temp.path().join("auth.json");
        write(
            &path,
            r#"{"openai":{"type":"api_key","key":"$SCOPED_KEY","env":{"SCOPED_KEY":"scoped-value"}}}"#,
        )?;
        let storage = AuthStorage::create(path);
        let credential = storage.get("openai").ok_or("missing credential")?;
        let Credential::ApiKey(api_key) = credential else {
            return Err("expected API key credential".to_string());
        };
        assert_eq!(api_key.key.as_deref(), Some("scoped-value"));
        assert_eq!(
            api_key.env.get("SCOPED_KEY").map(String::as_str),
            Some("scoped-value")
        );
        Ok(())
    }

    #[test]
    fn resolves_command_backed_keys() -> Result<(), String> {
        let temp = root()?;
        let path = temp.path().join("auth.json");
        write(
            &path,
            r#"{"openai":{"type":"api_key","key":"!printf 'command-key'"}}"#,
        )?;
        let storage = AuthStorage::create(path);
        assert!(storage.get("openai") == Some(api_key("command-key")));
        assert!(is_command_config_value("!printf x"));
        assert!(!is_command_config_value("$KEY"));
        Ok(())
    }

    #[test]
    fn oauth_credentials_are_returned_unchanged() -> Result<(), String> {
        let temp = root()?;
        let path = temp.path().join("auth.json");
        let credential = Credential::OAuth(pi_rs_ai::auth::OAuthCredential {
            access: "access".to_string(),
            refresh: "refresh".to_string(),
            expires: 99,
            details: serde_json::Map::new(),
        });
        let mut storage = AuthStorage::create(path);
        storage.set("openai-codex", credential.clone())?;
        assert!(storage.get("openai-codex") == Some(credential));
        Ok(())
    }

    #[test]
    fn malformed_auth_file_is_never_overwritten() -> Result<(), String> {
        let temp = root()?;
        let path = temp.path().join("auth.json");
        write(&path, "{invalid-json")?;
        let mut storage = AuthStorage::create(path.clone());
        assert!(storage.set("openai", api_key("new")).is_err());
        assert!(storage.delete("openai").is_err());
        assert_eq!(
            std::fs::read_to_string(&path).map_err(|e| e.to_string())?,
            "{invalid-json"
        );
        Ok(())
    }

    #[test]
    fn mutations_preserve_unrelated_external_edits() -> Result<(), String> {
        let temp = root()?;
        let path = temp.path().join("auth.json");
        let mut storage = AuthStorage::create(path.clone());
        write(
            &path,
            r#"{"openai":{"type":"api_key","key":"old"},"google":{"type":"api_key","key":"external"}}"#,
        )?;
        storage.set("openai", api_key("new"))?;
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        assert_eq!(saved["openai"]["key"], Value::String("new".into()));
        assert_eq!(saved["google"]["key"], Value::String("external".into()));
        assert!(storage.get("google") == Some(api_key("external")));
        Ok(())
    }

    #[test]
    fn parses_bom_prefixed_auth_file() -> Result<(), String> {
        let temp = root()?;
        let path = temp.path().join("auth.json");
        write(
            &path,
            "\u{feff}{\"openai\":{\"type\":\"api_key\",\"key\":\"bom-key\"}}",
        )?;
        let storage = AuthStorage::create(path);
        assert!(storage.get("openai") == Some(api_key("bom-key")));
        Ok(())
    }

    #[test]
    fn read_stored_credential_does_not_create_files() -> Result<(), String> {
        let temp = root()?;
        let path = temp.path().join("missing").join("auth.json");
        assert!(read_stored_credential("openai", &path).is_none());
        assert!(!path.exists());
        assert!(!path.parent().ok_or("missing parent")?.exists());
        Ok(())
    }

    #[test]
    fn with_lock_can_commit_raw_json() -> Result<(), String> {
        let temp = root()?;
        let path = temp.path().join("auth.json");
        let mut storage = AuthStorage::create(path.clone());
        let seen = storage.with_lock(|current| {
            assert_eq!(current, Some("{}"));
            Ok(LockResult {
                result: 7,
                next: Some(r#"{"openai":{"type":"api_key","key":"raw"}}"#.to_string()),
            })
        })?;
        assert_eq!(seen, 7);
        assert!(storage.get("openai") == Some(api_key("raw")));
        Ok(())
    }

    #[test]
    fn preserving_existing_file_mode_on_update() -> Result<(), String> {
        let temp = root()?;
        let path = temp.path().join("auth.json");
        write(&path, r#"{"openai":{"type":"api_key","key":"old"}}"#)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660))
                .map_err(|e| e.to_string())?;
        }
        let mut storage = AuthStorage::create(path.clone());
        storage.set("openai", api_key("new"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path)
                    .map_err(|e| e.to_string())?
                    .permissions()
                    .mode()
                    & 0o777,
                0o660
            );
        }
        Ok(())
    }

    #[test]
    fn missing_key_credential_is_returned_as_is() -> Result<(), String> {
        let temp = root()?;
        let path = temp.path().join("auth.json");
        write(&path, r#"{"openai":{"type":"api_key"}}"#)?;
        let storage = AuthStorage::create(path);
        assert!(
            storage.get("openai")
                == Some(Credential::ApiKey(
                    pi_rs_ai::auth::ApiKeyCredential::default()
                ))
        );
        Ok(())
    }
}
