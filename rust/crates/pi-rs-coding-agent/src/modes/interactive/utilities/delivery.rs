use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use pi_rs_agent::CancellationToken;
use pi_rs_ai::auth::{AuthOverrides, AuthResult};
use pi_rs_ai::{Models, ModelsOptions};
use serde_json::{Value, json};
use tokio::process::Command;
use url::Url;
use uuid::Uuid;

use super::editor::PrivateTempDir;
use crate::cli::auth_command::credential_store;
use crate::config::get_share_viewer_url;
use crate::core::agent_session::AgentSession;
use crate::core::export_html::{ExportOptions, SessionSnapshot, export_session_html};
use crate::core::session_manager::CURRENT_SESSION_VERSION;

mod bug;
pub use bug::{
    BUG_REPORT_DISCLAIMER, BUG_REPORT_TRANSCRIPT_NOTE, BugReportBundle, BugReportOptions,
    bug_report_gateway, build_bug_report, export_bug_report, summarize_bug_report,
    upload_bug_report,
};

const DEFAULT_RADIUS_GATEWAY: &str = "https://radius.pi.dev";
const RADIUS: &str = "radius";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ShareDestination {
    #[default]
    Auto,
    Radius,
    Gist,
}

impl ShareDestination {
    pub fn from_setting(value: Option<&str>) -> Self {
        match value {
            Some("radius") => Self::Radius,
            Some("gist") => Self::Gist,
            _ => Self::Auto,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ShareLinks {
    pub viewer: String,
    pub gist: Option<String>,
}

/// Share only after a user has agreed to upload the transcript. Auto falls back to a private
/// gist only when Radius is not authenticated, never after a Radius network error.
pub async fn share_session(
    session: &AgentSession,
    agent_dir: &Path,
    destination: ShareDestination,
    consent: bool,
    signal: &CancellationToken,
) -> Result<Option<ShareLinks>, String> {
    if !consent || signal.is_cancelled() {
        return Ok(None);
    }
    if offline() {
        return Err("Sharing sessions requires online mode. Use /export instead.".into());
    }
    let share = async {
        if destination != ShareDestination::Gist {
            let gateway = Url::parse(DEFAULT_RADIUS_GATEWAY).map_err(|error| error.to_string())?;
            if let Some(token) = radius_token(agent_dir, &gateway, signal).await? {
                let mut url = gateway
                    .join("/v1/artifacts")
                    .map_err(|error| error.to_string())?;
                url.query_pairs_mut()
                    .append_pair("visibility", "organization")
                    .append_pair("title", "Pi session");
                let body = serialize_session_branch(&export_snapshot(session)?)?;
                let response = upload_client()?
                    .post(url)
                    .bearer_auth(token)
                    .header(reqwest::header::CONTENT_TYPE, "application/x-ndjson")
                    .body(body)
                    .send()
                    .await
                    .map_err(|error| format!("Failed to upload Radius artifact: {error}"))?;
                let status = response.status();
                let data = response.json::<Value>().await.ok();
                if status.is_success()
                    && let Some(viewer) = data
                        .as_ref()
                        .and_then(|data| {
                            data.pointer("/artifact/canonical_url")
                                .and_then(Value::as_str)
                        })
                        .filter(|url| safe_link(url))
                {
                    return Ok(Some(ShareLinks {
                        viewer: viewer.to_owned(),
                        gist: None,
                    }));
                }
                let detail = data
                    .as_ref()
                    .and_then(|data| data.get("error"))
                    .and_then(Value::as_str);
                return Err(format!(
                    "Failed to upload Radius artifact: {}",
                    detail.map_or_else(|| status.to_string(), str::to_owned)
                ));
            }
            if destination == ShareDestination::Radius {
                return Err(
                    "Radius authentication is not configured. Run '/login radius' first.".into(),
                );
            }
        }
        share_via_gist(session, agent_dir).await.map(Some)
    };
    tokio::select! {
        biased;
        _ = signal.cancelled() => Ok(None),
        result = share => result,
    }
}

async fn share_via_gist(session: &AgentSession, agent_dir: &Path) -> Result<ShareLinks, String> {
    let auth = gh()
        .args(["auth", "status"])
        .output()
        .await
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                "GitHub CLI (gh) is not installed. Install it from https://cli.github.com/"
                    .to_owned()
            } else {
                format!("Failed to start GitHub CLI: {error}")
            }
        })?;
    if !auth.status.success() {
        return Err("GitHub CLI is not logged in. Run 'gh auth login' first.".into());
    }
    let snapshot = export_snapshot(session)?;
    if snapshot.entries.is_empty() {
        return Err("Nothing to export yet - start a conversation first".into());
    }
    let cwd = session.session_manager().cwd();
    let settings = crate::core::settings_manager::SettingsManager::create_with_dirs(
        Path::new(&cwd),
        agent_dir,
        crate::core::settings_manager::CreateOptions {
            project_trusted: false,
        },
    );
    let theme_name = settings
        .get_settings()
        .get("theme")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let html = export_session_html(
        &snapshot,
        &ExportOptions {
            theme_name,
            ..ExportOptions::default()
        },
    )?;
    let directory = PrivateTempDir::new("pi-share").map_err(|error| error.to_string())?;
    let file = directory.path().join("session.html");
    tokio::fs::write(&file, html)
        .await
        .map_err(|error| format!("Failed to export session: {error}"))?;
    let output = gh()
        .args(["gist", "create", "--public=false"])
        .arg(file)
        .output()
        .await
        .map_err(|error| format!("Failed to create gist: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Failed to create gist: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    gist_links(String::from_utf8_lossy(&output.stdout).trim())
}

fn gh() -> Command {
    let mut command = Command::new("gh");
    command.stdin(Stdio::null()).kill_on_drop(true);
    command
}

fn gist_links(raw: &str) -> Result<ShareLinks, String> {
    let url = Url::parse(raw)
        .ok()
        .filter(|url| url.scheme() == "https" && url.host_str().is_some());
    let gist_id = url
        .as_ref()
        .and_then(Url::path_segments)
        .and_then(|mut segments| segments.next_back())
        .filter(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_alphanumeric()))
        .ok_or("Failed to parse gist ID from gh output")?;
    Ok(ShareLinks {
        viewer: get_share_viewer_url(gist_id),
        gist: Some(raw.to_owned()),
    })
}

fn safe_link(raw: &str) -> bool {
    !raw.chars().any(char::is_control)
        && Url::parse(raw)
            .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
}

fn offline() -> bool {
    std::env::var_os("PI_OFFLINE").is_some()
}

fn native_models(agent_dir: &Path) -> Result<Models, String> {
    Models::builtin(ModelsOptions {
        credentials: credential_store(agent_dir),
        ..ModelsOptions::default()
    })
    .map_err(|error| error.to_string())
}

async fn radius_token(
    agent_dir: &Path,
    gateway: &Url,
    signal: &CancellationToken,
) -> Result<Option<String>, String> {
    let models = native_models(agent_dir)?;
    models.set_provider(
        pi_rs_ai::providers::radius_provider(RADIUS, "Radius", gateway.clone())
            .map_err(|error| error.to_string())?,
    );
    let auth = models
        .get_auth(
            RADIUS,
            &AuthOverrides {
                min_oauth_validity: Some(Duration::from_secs(300)),
                signal: signal.clone(),
                ..AuthOverrides::default()
            },
        )
        .await
        .map_err(|error| error.to_string())?;
    Ok(auth.and_then(auth_token))
}

fn auth_token(result: AuthResult) -> Option<String> {
    result
        .auth
        .api_key
        .filter(|key| !key.is_empty())
        .or_else(|| {
            result.auth.headers?.into_iter().find_map(|(name, value)| {
                if !name.eq_ignore_ascii_case("authorization") {
                    return None;
                }
                let (scheme, token) = value.as_deref()?.split_once(char::is_whitespace)?;
                let token = token.trim_start();
                (!token.is_empty() && scheme.eq_ignore_ascii_case("bearer"))
                    .then(|| token.to_owned())
            })
        })
}

fn upload_client() -> Result<reqwest::Client, String> {
    // An automatic cross-host redirect could forward a transcript to an unconfirmed sender.
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| error.to_string())
}

fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn to_json<T: serde::Serialize>(value: T) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|error| error.to_string())
}

/// Snapshot one active branch; do not disclose other branches in either share format.
fn export_snapshot(session: &AgentSession) -> Result<SessionSnapshot, String> {
    let manager = session.session_manager();
    let now = timestamp();
    let header = json!({
        "type": "session", "version": CURRENT_SESSION_VERSION, "id": manager.session_id(),
        "timestamp": now, "cwd": manager.cwd(),
    });
    let mut entries = if let Some(disk) = manager.disk() {
        disk.lock()
            .get_branch(None)
            .into_iter()
            .map(to_json)
            .collect::<Result<Vec<_>, _>>()?
    } else {
        manager
            .entries()
            .into_iter()
            .map(|entry| {
                let mut data = if entry.kind == "message" {
                    json!({"message": entry.data})
                } else {
                    entry.data
                };
                if let Some(data) = data.as_object_mut() {
                    data.insert("type".into(), Value::from(entry.kind));
                    data.insert("id".into(), Value::from(entry.id));
                    data.insert("timestamp".into(), Value::from(now.as_str()));
                    if let Some(level) = data.remove("level") {
                        data.insert("thinkingLevel".into(), level);
                    }
                }
                data
            })
            .collect()
    };
    let mut parent_id = Value::Null;
    for entry in &mut entries {
        if let Some(entry) = entry.as_object_mut() {
            let id = entry.get("id").cloned().unwrap_or_default();
            entry.insert("parentId".into(), parent_id);
            parent_id = id;
        }
    }
    let system_prompt = session.system_prompt();
    let tools = session.interactive_tools().into_iter().map(|tool| {
        json!({"name": tool.name, "description": tool.description, "parameters": tool.parameters})
    }).collect::<Vec<_>>();
    let leaf_id = parent_id.as_str().map(str::to_owned);
    Ok(SessionSnapshot {
        header,
        entries,
        leaf_id,
        system_prompt: Some(system_prompt),
        tools,
    })
}

fn serialize_session_branch(snapshot: &SessionSnapshot) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    for value in std::iter::once(&snapshot.header).chain(&snapshot.entries) {
        serde_json::to_writer(&mut bytes, value).map_err(|error| error.to_string())?;
        bytes.push(b'\n');
    }
    if let Some(system_prompt) = snapshot.system_prompt.as_deref() {
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct ShareMetadata<'a> {
            system_prompt: &'a str,
            tools: &'a [Value],
        }

        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct TrailingEntry<'a> {
            r#type: &'static str,
            custom_type: &'static str,
            id: &'a str,
            parent_id: Option<&'a str>,
            timestamp: &'a Value,
            data: ShareMetadata<'a>,
        }

        let mut buffer = Uuid::encode_buffer();
        let uuid = Uuid::new_v4().simple().encode_lower(&mut buffer);
        let entry = TrailingEntry {
            r#type: "custom",
            custom_type: "pi.share",
            id: &uuid[..8],
            parent_id: snapshot.leaf_id.as_deref(),
            timestamp: &snapshot.header["timestamp"],
            data: ShareMetadata {
                system_prompt,
                tools: &snapshot.tools,
            },
        };
        serde_json::to_writer(&mut bytes, &entry).map_err(|error| error.to_string())?;
        bytes.push(b'\n');
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pi_rs_agent::{Agent, AgentInitialState, AgentOptions};
    use pi_rs_ai::faux::{FauxOptions, faux_provider};

    use crate::core::agent_session::{
        AgentSessionConfig, InMemoryModelRegistry, InMemorySettings, NoopExtensionRunner,
        StaticAuth, TailCompactor,
    };
    use crate::core::agent_session_services::DiskSession;
    use crate::core::session_manager::SessionManager;

    use super::*;

    #[test]
    fn gist_output_requires_https_and_a_final_identifier() -> Result<(), String> {
        assert!(gist_links("https://gist.github.com/user/").is_err());
        assert!(gist_links("file:///abc").is_err());
        let links = gist_links("https://gist.github.com/user/abc789")?;
        assert_eq!(
            links.gist.as_deref(),
            Some("https://gist.github.com/user/abc789")
        );
        assert!(links.viewer.ends_with("abc789"));
        Ok(())
    }

    #[test]
    fn active_branch_jsonl_has_one_record_per_line_and_a_trailing_newline() -> Result<(), String> {
        let snapshot = SessionSnapshot {
            header: json!({"type":"session"}),
            entries: vec![json!({"type": "message", "data": "a\nb"})],
            ..SessionSnapshot::default()
        };
        assert_eq!(
            serialize_session_branch(&snapshot)?,
            b"{\"type\":\"session\"}\n{\"type\":\"message\",\"data\":\"a\\nb\"}\n"
        );
        Ok(())
    }

    fn test_session() -> Result<(AgentSession, Arc<DiskSession>), String> {
        let faux = faux_provider(FauxOptions::default());
        let model = faux.get_model(None).ok_or("Missing faux model")?.clone();
        let models = Models::default();
        models.set_provider(faux.provider);
        let agent = Agent::new(AgentOptions {
            initial_state: AgentInitialState {
                model: Some(model.clone()),
                system_prompt: Some("sample system prompt".into()),
                ..AgentInitialState::default()
            },
            stream_fn: Some(pi_rs_agent::models_stream_fn(models)),
            ..AgentOptions::default()
        })
        .map_err(|error| error.to_string())?;
        let manager = Arc::new(DiskSession::new(SessionManager::in_memory()));
        let auth = Arc::new(StaticAuth::allow_all());
        let session = AgentSession::new(AgentSessionConfig {
            agent,
            session_manager: manager.clone(),
            settings: Arc::new(InMemorySettings::default()),
            models: Arc::new(InMemoryModelRegistry::new(vec![model], auth.clone())),
            auth,
            compactor: Arc::new(TailCompactor { keep_recent: 1 }),
            extensions: Arc::new(NoopExtensionRunner),
            cwd: String::new(),
            retry_prompt: None,
        })
        .map_err(|error| error.to_string())?;
        Ok((session, manager))
    }

    #[test]
    fn share_snapshot_exports_only_active_branch() -> Result<(), String> {
        let (session, manager) = test_session()?;
        let (root, leaf) = {
            let mut disk = manager.lock();
            let root =
                disk.append_message(json!({"role": "user", "content": "root", "timestamp": 0}))?;
            disk.append_message(
                json!({"role": "user", "content": "unshared branch", "timestamp": 1}),
            )?;
            disk.branch(&root)?;
            let leaf = disk.append_message(
                json!({"role": "user", "content": "shared branch", "timestamp": 2}),
            )?;
            (root, leaf)
        };
        let snapshot = export_snapshot(&session)?;
        let body = String::from_utf8(serialize_session_branch(&snapshot)?)
            .map_err(|error| error.to_string())?;
        assert!(body.contains("shared branch"));
        assert!(!body.contains("unshared branch"));
        assert!(body.contains("sample system prompt"));
        assert_eq!(snapshot.entries[0]["parentId"], Value::Null);
        assert_eq!(snapshot.entries[1]["parentId"], root);
        assert_eq!(snapshot.leaf_id.as_deref(), Some(leaf.as_str()));
        let shared = SessionSnapshot::from_jsonl(&body)?;
        assert_eq!(shared.entries[2]["parentId"], leaf);
        assert_eq!(shared.entries[2]["customType"], "pi.share");
        Ok(())
    }

    #[tokio::test]
    async fn sharing_and_reporting_refuse_without_consent() -> Result<(), String> {
        let (session, _) = test_session()?;
        let root = tempfile::tempdir().map_err(|error| error.to_string())?;
        let signal = CancellationToken::new();
        assert!(
            share_session(
                &session,
                root.path(),
                ShareDestination::Radius,
                false,
                &signal
            )
            .await?
            .is_none()
        );
        assert!(
            summarize_bug_report(&session, root.path(), None, false, &signal)
                .await?
                .is_none()
        );
        let settings = crate::core::settings_manager::SettingsManager::in_memory(
            Value::Null,
            Default::default(),
        );
        let bundle = build_bug_report(&session, &settings, &BugReportOptions::default(), None)?;
        assert!(
            upload_bug_report(&session, &bundle, root.path(), false, &signal)
                .await?
                .is_none()
        );
        assert!(!root.path().join("auth.json").exists());
        assert!(!bundle.id().is_empty());
        Ok(())
    }
}
