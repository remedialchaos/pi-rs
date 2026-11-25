use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use pi_rs_ai::{
    AssistantContent, Context, Message, Model, ProviderRequestOptions, StopReason, StreamOptions,
    StringOrBlocks, UserMessage, UserRole,
};
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

use super::*;
use crate::config::{VERSION, get_agent_dir};
use crate::core::compaction::{estimate_message_tokens, serialize_conversation};
use crate::core::messages::convert_to_llm;
use crate::core::model_registry::ModelRegistry;
use crate::core::settings_manager::SettingsManager;
use crate::utils::pi_user_agent::pi_user_agent;

pub const BUG_REPORT_DISCLAIMER: &str = "This report goes to the Pi developers (Earendil) and is not shared publicly. It includes your pi version, operating system, the current model and provider configuration (without API keys), loaded extensions, settings, and provider error diagnostics from this session.";
pub const BUG_REPORT_TRANSCRIPT_NOTE: &str = "The transcript contains your messages, model output, tool calls and their results, including file contents and command output read during this session.";
const REPORT_ENTRY: &str = "pi.bug-report";

#[derive(Clone, Debug, Default)]
pub struct BugReportOptions {
    pub hint: Option<String>,
    pub include_session: bool,
    pub include_summary: bool,
}

pub struct BugReportBundle {
    id: String,
    metadata: Value,
    diagnostics: Value,
    transcript: Option<Vec<u8>>,
    summary: Option<String>,
    crash_log: Option<(PathBuf, Vec<u8>)>,
}

#[derive(Debug, Clone, Default)]
pub struct BugReportUploadOptions {
    pub consent: bool,
    pub gateway_override: Option<String>,
    pub offline_override: Option<bool>,
}

/// Gateway named explicitly by the confirmation dialog before upload.
pub fn bug_report_gateway() -> Result<Url, String> {
    radius_gateway(None)
}

/// Generate a summary only after the user agrees to send transcript content to the model provider.
pub async fn summarize_bug_report(
    session: &AgentSession,
    agent_dir: &Path,
    hint: Option<&str>,
    consent: bool,
    signal: &CancellationToken,
) -> Result<Option<String>, String> {
    if !consent || signal.is_cancelled() {
        return Ok(None);
    }
    if offline() {
        return Err("Writing a bug report summary requires online mode".into());
    }
    let model = session.model();
    let registry = ModelRegistry::new(
        native_models(agent_dir)?,
        Some(agent_dir.join("models.json")),
    );
    let context_window = if model.context_window > 0 {
        model.context_window
    } else {
        128_000
    };
    let options = StreamOptions {
        reasoning: (model.reasoning && session.thinking_level() != pi_rs_ai::ThinkingLevel::Off)
            .then(|| session.thinking_level()),
        session_id: Some(session.session_id()),
        max_tokens: Some(if model.max_tokens > 0 {
            model.max_tokens.min(4096)
        } else {
            4096
        }),
        request: ProviderRequestOptions {
            api_key: registry.api_key_for_provider(&model.provider),
            signal: signal.clone(),
            ..ProviderRequestOptions::default()
        },
        ..StreamOptions::default()
    };
    let messages = convert_to_llm(&session.messages())
        .into_iter()
        .map(to_json)
        .collect::<Result<Vec<_>, _>>()?;
    let prompt = summary_prompt(&messages, context_window.saturating_mul(3) / 5, hint);
    let context = Context {
        system_prompt: Some("You are helping a user file a bug report about Pi, the coding agent. You will be shown the conversation transcript. Write a report for the Pi developers describing what the user was doing and what went wrong. Do not continue the conversation or answer questions in it. Output only the report.".into()),
        messages: vec![Message::User(UserMessage {
            role: UserRole::User,
            content: StringOrBlocks::String(prompt),
            timestamp: 0,
        })],
        tools: None,
    };
    let response = tokio::select! {
        biased;
        _ = signal.cancelled() => return Ok(None),
        result = registry.models().complete_simple(model, context, options) => result,
    }
    .map_err(|error| format!("Failed to write bug report summary: {error}"))?;
    if response.stop_reason == StopReason::Aborted {
        return Ok(None);
    }
    if response.stop_reason.failed() {
        return Err(format!(
            "Failed to write bug report summary: {}",
            response
                .error_message
                .as_deref()
                .unwrap_or("unknown provider error")
        ));
    }
    let mut text = String::new();
    for block in &response.content {
        match block {
            AssistantContent::Text(block) => text.push_str(&block.text),
            AssistantContent::ToolCall(_) => {
                return Err("Bug report summary attempted to call a tool".into());
            }
            _ => {}
        }
    }
    let summary = text.trim();
    if summary.is_empty() {
        return Err("Bug report summary was empty".into());
    }
    Ok(Some(summary.to_owned()))
}

fn summary_prompt(messages: &[Value], budget: u64, hint: Option<&str>) -> String {
    let mut keep_from = messages.len();
    let mut remaining = budget;
    for (index, message) in messages.iter().enumerate().rev() {
        let next = estimate_message_tokens(message) as u64;
        if keep_from != messages.len() && next > remaining {
            break;
        }
        keep_from = index;
        remaining = remaining.saturating_sub(next);
    }
    let mut prompt = if keep_from > 0 {
        format!(
            "Note: only the last {} of {} messages are shown.\n\n",
            messages.len() - keep_from,
            messages.len()
        )
    } else {
        String::new()
    };
    prompt.push_str("<conversation>\n");
    prompt.push_str(&serialize_conversation(&messages[keep_from..]));
    prompt.push_str("\n</conversation>\n\n");
    if let Some(hint) = hint.map(str::trim).filter(|hint| !hint.is_empty()) {
        prompt.push_str("<user-report>\n");
        prompt.push_str(hint);
        prompt.push_str("\n</user-report>\n\n");
    }
    prompt.push_str("Write the bug report in Markdown with sections: What the user was doing; What went wrong (quote any errors); Steps to reproduce (numbered); Relevant details. Do not include file contents, secrets, or credentials. Refer to files by path only. Keep it factual and concise.");
    prompt
}

/// Collect redacted metadata separately from the optional, explicitly selected transcript.
pub fn build_bug_report(
    session: &AgentSession,
    settings: &SettingsManager,
    options: &BugReportOptions,
    summary: Option<String>,
) -> Result<BugReportBundle, String> {
    if options.include_summary != summary.is_some()
        || (options.include_session && options.include_summary)
    {
        return Err(
            "Bug report must contain only the transcript or summary the user selected".into(),
        );
    }
    let id = Uuid::new_v4().to_string();
    let model = session.model();
    let manager = session.session_manager();
    let mut session_info = json!({
        "id": manager.session_id(), "included": options.include_session,
        "summaryIncluded": summary.is_some(), "messageCount": manager.stats().total_messages,
    });
    if options.include_session {
        session_info["cwd"] = Value::from(manager.cwd());
    }
    let provider = get_agent_dir()
        .ok()
        .and_then(|dir| native_models(&dir).ok())
        .and_then(|models| models.get_provider(&model.provider));
    let provider = provider.map(|provider| {
        let mut auth = Vec::new();
        if provider.auth.api_key.is_some() { auth.push("api_key"); }
        if provider.auth.oauth.is_some() { auth.push("oauth"); }
        let mut headers = provider.headers.as_ref().into_iter().flat_map(|headers| headers.keys()).collect::<Vec<_>>();
        headers.sort_unstable();
        json!({"id": provider.id, "name": provider.name,
            "baseUrl": provider.base_url.as_deref().map(|url| redact_url(url).unwrap_or_else(|| url.to_owned())),
            "headerNames": headers, "authTypes": auth})
    });
    let (mut extensions, mut extension_errors) = (Vec::new(), Vec::new());
    if let Some(resources) = session.interactive_resources() {
        for extension in resources.extensions.extensions {
            let registration = extension.registration();
            extensions.push(json!({
                "path": registration.path,
                "hidden": registration.hidden,
            }));
        }
        for error in resources.extensions.errors {
            extension_errors.push(json!({
                "path": error.extension_path,
                "error": error.error,
            }));
        }
        for extension in extensions.iter_mut().chain(&mut extension_errors) {
            redact_json(extension);
        }
    }
    let metadata = json!({
        "schemaVersion": 1, "id": id, "createdAt": timestamp(),
        "hint": options.hint.as_deref().map(str::trim).filter(|hint| !hint.is_empty()),
        "environment": environment(), "session": session_info,
        "model": describe_model(&model), "provider": provider,
        "thinkingLevel": session.thinking_level(),
        "extensions": extensions, "extensionErrors": extension_errors,
        "settings": {"global": redact_settings(settings.get_global_settings()),
            "project": redact_settings(settings.get_project_settings())},
    });
    let crash_log = get_agent_dir()
        .ok()
        .map(|path| path.join("crashes.json"))
        .and_then(|path| {
            let data = std::fs::read(&path).ok()?;
            Some((path, data))
        });
    let crashes = crash_log
        .as_ref()
        .and_then(|(_, data)| serde_json::from_slice::<Vec<Value>>(data).ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|mut crash| {
            let object = crash.as_object_mut()?;
            if !object.get("message").is_some_and(Value::is_string)
                || !object.get("timestamp").is_some_and(Value::is_string)
            {
                return None;
            }
            object.shift_remove("notified");
            redact_json(&mut crash);
            Some(crash)
        })
        .collect::<Vec<_>>();
    let mut assistant = Vec::new();
    let mut assistant_count = 0;
    let mut collect = |id: &str, timestamp: &str, message: &Value| {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            return;
        }
        assistant_count += 1;
        let diagnostics = message.get("diagnostics").and_then(Value::as_array);
        if !diagnostics.is_some_and(|value| !value.is_empty())
            && !matches!(
                message.get("stopReason").and_then(Value::as_str),
                Some("error" | "aborted")
            )
            && !message.get("errorMessage").is_some_and(Value::is_string)
        {
            return;
        }
        let mut result = json!({"entryId": id, "timestamp": timestamp,
            "diagnostics": diagnostics.map_or(&[][..], Vec::as_slice)});
        for key in [
            "provider",
            "model",
            "api",
            "stopReason",
            "rawStopReason",
            "errorMessage",
        ] {
            if let Some(value) = message.get(key) {
                result[key] = value.clone();
            }
        }
        redact_json(&mut result);
        assistant.push(result);
    };
    let entry_count = if let Some(disk) = manager.disk() {
        let disk = disk.lock();
        let entries = disk.get_entries();
        for entry in entries {
            if let crate::core::session_manager::SessionEntry::Message {
                id,
                timestamp,
                message,
                ..
            } = entry
            {
                collect(id, timestamp, message);
            }
        }
        entries.len()
    } else {
        let entries = manager.entries();
        for entry in &entries {
            if entry.kind == "message" {
                collect(&entry.id, "", &entry.data);
            }
        }
        entries.len()
    };
    let crash_log = crash_log.filter(|_| !crashes.is_empty());
    let diagnostics = json!({"schemaVersion": 1, "sessionId": manager.session_id(), "entryCount": entry_count,
        "assistantMessageCount": assistant_count, "assistant": assistant, "crashes": crashes});
    let transcript = options
        .include_session
        .then(|| export_snapshot(session).and_then(|snapshot| serialize_session_branch(&snapshot)))
        .transpose()?;
    Ok(BugReportBundle {
        id,
        metadata,
        diagnostics,
        transcript,
        summary,
        crash_log,
    })
}

fn environment() -> Value {
    let env = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
    let mut names = std::env::vars_os()
        .filter_map(|(name, _)| name.into_string().ok())
        .filter(|name| name.starts_with("PI_"))
        .collect::<Vec<_>>();
    names.sort_unstable();
    json!({
        "version": VERSION, "userAgent": pi_user_agent(VERSION), "runtime": "rust",
        "platform": std::env::consts::OS, "arch": std::env::consts::ARCH,
        "osRelease": std::fs::read_to_string("/proc/sys/kernel/osrelease").ok().map(|data| data.trim().to_owned()),
        "osVersion": std::fs::read_to_string("/proc/sys/kernel/version").ok().map(|data| data.trim().to_owned()),
        "shell": env("SHELL").and_then(|value| value.rsplit(['/', '\\']).next().map(str::to_owned)),
        "terminal": {"term": env("TERM"), "program": env("TERM_PROGRAM"),
            "programVersion": env("TERM_PROGRAM_VERSION"), "colorterm": env("COLORTERM"),
            "tmux": env("TMUX").is_some(), "ssh": (["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"].iter().any(|key| env(key).is_some())),
            "ci": env("CI").is_some()},
        "piEnvironmentVariables": names,
    })
}

fn describe_model(model: &Model) -> Value {
    let mut headers = model
        .headers
        .as_ref()
        .into_iter()
        .flat_map(|headers| headers.keys())
        .collect::<Vec<_>>();
    headers.sort_unstable();
    let mut data = json!({
        "id": model.id, "name": model.name, "provider": model.provider, "api": model.api,
        "baseUrl": model.base_url, "input": model.input, "reasoning": model.reasoning,
        "contextWindow": model.context_window, "maxTokens": model.max_tokens,
        "samplingParams": model.sampling_params, "compat": model.compat,
        "thinkingLevelMap": model.thinking_level_map, "headerNames": headers,
    });
    redact_json(&mut data);
    data
}

fn sensitive_key(key: &str) -> bool {
    let mut normalized = String::with_capacity(key.len());
    let mut previous_small = false;
    for ch in key.chars() {
        if previous_small && ch.is_ascii_uppercase() {
            normalized.push('_');
        }
        previous_small = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        normalized.push(ch.to_ascii_lowercase());
    }
    let mut previous = "";
    for word in normalized.split(['-', '_']) {
        if matches!(
            word,
            "apikey"
                | "secret"
                | "token"
                | "password"
                | "passwd"
                | "credential"
                | "authorization"
                | "cookie"
        ) || (word == "key" && previous == "api")
        {
            return true;
        }
        previous = word;
    }
    false
}

fn redact_url(raw: &str) -> Option<String> {
    let (scheme, tail) = raw.split_once(':')?;
    if !tail.starts_with("//")
        && scheme
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'.' | b'-'))
        && let Some((_, after_scheme)) = tail.split_once(':')
        && after_scheme.starts_with("//")
    {
        return redact_url(tail).map(|url| format!("{scheme}:{url}"));
    }
    let mut url = Url::parse(raw).ok()?;
    let has_credentials = !url.username().is_empty() || url.password().is_some();
    let redact_params = url.query_pairs().any(|(key, _)| sensitive_key(&key));
    if !has_credentials && !redact_params {
        return None;
    }
    if has_credentials {
        url.set_username("").ok()?;
        url.set_password(None).ok()?;
    }
    if redact_params {
        let query = url
            .query_pairs()
            .into_owned()
            .map(|(key, value)| {
                let value = if sensitive_key(&key) {
                    "<redacted>".to_owned()
                } else {
                    value
                };
                (key, value)
            })
            .collect::<Vec<_>>();
        url.query_pairs_mut().clear().extend_pairs(query);
    }
    Some(url.into())
}

fn redact_json(value: &mut Value) {
    match value {
        Value::Object(values) => {
            for (key, value) in values {
                if !value.is_null() && sensitive_key(key) {
                    *value = Value::from("<redacted>");
                } else {
                    redact_json(value);
                }
            }
        }
        Value::Array(values) => values.iter_mut().for_each(redact_json),
        Value::String(text) => {
            if let Some(redacted) = redact_url(text) {
                *text = redacted;
            }
            let inline = redact_inline_secrets(text);
            if inline != *text {
                *text = inline;
            }
        }
        _ => {}
    }
}

fn redact_inline_secrets(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        if ["http://", "https://", "ssh://", "git://"]
            .into_iter()
            .any(|scheme| {
                rest.get(..scheme.len())
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(scheme))
            })
        {
            let url_len = rest
                .find(|ch: char| {
                    ch.is_whitespace() || matches!(ch, '"' | '\'' | ',' | ';' | ')' | ']' | '>')
                })
                .unwrap_or(rest.len());
            if let Some(redacted) = redact_url(&rest[..url_len]) {
                out.push_str(&redacted);
                rest = &rest[url_len..];
                continue;
            }
        }

        let lower_bearer = rest
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("bearer "));
        if lower_bearer {
            let after_prefix = &rest[7..];
            let token_len = after_prefix
                .find(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | ',' | ';'))
                .unwrap_or(after_prefix.len());
            if token_len > 0 {
                out.push_str(&rest[..7]);
                out.push_str("<redacted>");
                rest = &after_prefix[token_len..];
                continue;
            }
        }

        if let Some(prefix) = [
            "sk-ant-",
            "sk-proj-",
            "sk-",
            "ghp_",
            "gho_",
            "github_pat_",
            "xoxb-",
            "AIza",
        ]
        .into_iter()
        .find(|prefix| rest.starts_with(prefix))
        {
            let tail = &rest[prefix.len()..];
            let token_len = tail
                .find(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-')))
                .unwrap_or(tail.len());
            if token_len >= 8 {
                out.push_str("<redacted>");
                rest = &tail[token_len..];
                continue;
            }
        }

        if let Some(eq_idx) = rest.find('=') {
            let candidate_key = &rest[..eq_idx];
            if !candidate_key.is_empty()
                && candidate_key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
                && sensitive_key(candidate_key)
            {
                let after_eq = &rest[eq_idx + 1..];
                let val_len = after_eq
                    .find(|ch: char| {
                        ch.is_whitespace() || matches!(ch, '&' | '"' | '\'' | ',' | ';')
                    })
                    .unwrap_or(after_eq.len());
                if val_len > 0 {
                    out.push_str(candidate_key);
                    out.push_str("=<redacted>");
                    rest = &after_eq[val_len..];
                    continue;
                }
            }
        }

        let Some(ch) = rest.chars().next() else {
            break;
        };
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    out
}

fn redact_settings(mut value: Value) -> Value {
    if let Some(settings) = value.as_object_mut() {
        settings.shift_remove("trackingId");
        settings.shift_remove("deviceId");
    }
    redact_json(&mut value);
    value
}

impl BugReportBundle {
    pub fn id(&self) -> &str {
        &self.id
    }

    fn files(&self) -> Result<Vec<ReportFile<'_>>, String> {
        use std::borrow::Cow;
        let pretty = |value| {
            let mut data = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
            data.push(b'\n');
            Ok::<_, String>(Cow::Owned(data))
        };
        let mut files = Vec::with_capacity(
            2 + usize::from(self.transcript.is_some()) + usize::from(self.summary.is_some()),
        );
        files.push(("report.json", "application/json", pretty(&self.metadata)?));
        files.push((
            "diagnostics.json",
            "application/json",
            pretty(&self.diagnostics)?,
        ));
        if let Some(transcript) = &self.transcript {
            files.push((
                "session.jsonl",
                "application/x-ndjson",
                Cow::Borrowed(transcript.as_slice()),
            ));
        }
        if let Some(summary) = &self.summary {
            let data = if summary.ends_with('\n') {
                Cow::Borrowed(summary.as_bytes())
            } else {
                let mut data = Vec::with_capacity(summary.len() + 1);
                data.extend_from_slice(summary.as_bytes());
                data.push(b'\n');
                Cow::Owned(data)
            };
            files.push(("summary.md", "text/markdown", data));
        }
        Ok(files)
    }
}

type ReportFile<'a> = (&'static str, &'static str, std::borrow::Cow<'a, [u8]>);

/// Export locally. Report archives are exclusively created and owner-only on Unix.
pub fn export_bug_report(
    session: &AgentSession,
    bundle: &BugReportBundle,
    directory: &Path,
) -> Result<PathBuf, String> {
    let path = directory.join(format!("pi-bug-report-{}.zip", bundle.id));
    write_report_zip(&path, &bundle.files()?)?;
    record_delivery(session, bundle, "zip", Some(&path));
    Ok(path)
}

fn write_report_zip(path: &Path, files: &[ReportFile<'_>]) -> Result<(), String> {
    let mut open = OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.mode(0o600);
    }
    let file = open
        .open(path)
        .map_err(|error| format!("Failed to create bug report: {error}"))?;
    let result = (|| {
        let mut writer = ZipWriter::new(file);
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .unix_permissions(0o600);
        for (name, _, data) in files {
            writer
                .start_file(*name, options)
                .map_err(|error| error.to_string())?;
            writer.write_all(data).map_err(|error| error.to_string())?;
        }
        writer.finish().map_err(|error| error.to_string())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}

/// Upload after explicit delivery consent; failures leave the bundle available for zip export.
pub async fn upload_bug_report(
    session: &AgentSession,
    bundle: &BugReportBundle,
    agent_dir: &Path,
    consent: bool,
    signal: &CancellationToken,
) -> Result<Option<String>, String> {
    upload_bug_report_with_options(
        session,
        bundle,
        agent_dir,
        &BugReportUploadOptions {
            consent,
            gateway_override: None,
            offline_override: None,
        },
        signal,
    )
    .await
}

/// Upload a bug report bundle with explicit gateway/offline overrides (primarily for testing).
pub async fn upload_bug_report_with_options(
    session: &AgentSession,
    bundle: &BugReportBundle,
    agent_dir: &Path,
    options: &BugReportUploadOptions,
    signal: &CancellationToken,
) -> Result<Option<String>, String> {
    if !options.consent || signal.is_cancelled() {
        return Ok(None);
    }
    if options.offline_override.unwrap_or_else(offline) {
        return Err(
            "Uploading bug reports requires online mode. Use Export as Zip instead.".into(),
        );
    }
    let gateway = radius_gateway(options.gateway_override.as_deref())?;
    let url = gateway
        .join("/v1/bug-reports")
        .map_err(|error| error.to_string())?;
    let upload = async {
        let token = radius_token(agent_dir, &gateway, signal).await?;
        let boundary = format!("pi-report-{}", Uuid::new_v4());
        let body = multipart(&bundle.files()?, &boundary)?;
        let mut request = upload_client_for(&gateway)?
            .post(url)
            .header(
                reqwest::header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .map_err(|error| format!("Bug report upload failed: {error}"))?;
        let status = response.status();
        let body = response.json::<Value>().await.ok();
        if let Some(body) = &body
            && status.is_success()
            && body.get("ok").and_then(Value::as_bool) == Some(true)
            && let Some(id) = body.pointer("/bug_report/id").and_then(Value::as_str)
        {
            return Ok(id.to_owned());
        }
        let detail = body
            .as_ref()
            .and_then(|body| body.get("description").or_else(|| body.get("error")))
            .and_then(Value::as_str);
        Err(format!(
            "Bug report upload failed: {}",
            detail.map_or_else(|| status.to_string(), str::to_owned)
        ))
    };
    let id = tokio::select! {
        biased;
        _ = signal.cancelled() => return Ok(None),
        result = upload => result?,
    };
    record_delivery(session, bundle, "upload", None);
    Ok(Some(id))
}

fn multipart(files: &[ReportFile<'_>], boundary: &str) -> Result<Vec<u8>, String> {
    let mut data = Vec::with_capacity(
        files
            .iter()
            .map(|(_, _, content)| content.len())
            .sum::<usize>()
            + files.len() * 160
            + boundary.len(),
    );
    for (name, content_type, content) in files {
        write!(data, "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{name}\"\r\nContent-Type: {content_type}\r\n\r\n")
            .map_err(|error| error.to_string())?;
        data.extend_from_slice(content);
        data.extend_from_slice(b"\r\n");
    }
    write!(data, "--{boundary}--\r\n").map_err(|error| error.to_string())?;
    Ok(data)
}

fn record_delivery(
    session: &AgentSession,
    bundle: &BugReportBundle,
    delivery: &str,
    path: Option<&Path>,
) {
    if let Some(disk) = session.session_manager().disk() {
        let mut data = json!({"id": bundle.id, "createdAt": bundle.metadata.get("createdAt"),
            "hint": bundle.metadata.get("hint"), "sessionIncluded": bundle.transcript.is_some(),
            "summaryIncluded": bundle.summary.is_some(), "delivery": delivery});
        if let Some(path) = path {
            data["path"] = Value::from(path.to_string_lossy().as_ref());
        }
        let _ = disk.lock().append_custom_entry(REPORT_ENTRY, Some(data));
    }
    if let Some((path, snapshot)) = &bundle.crash_log
        && std::fs::read(path).is_ok_and(|current| current == *snapshot)
    {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;

    #[test]
    fn settings_redact_camel_case_credentials_and_nested_source_urls() {
        let redacted = redact_settings(json!({"trackingId": "tracking", "deviceId": "device",
            "nested": [{"apiKey": "secret", "X-Auth-Token": "secret", "path": "git:https://alice:secret@example.test/repo?access_token=secret&ref=main"}],
            "unrelated": "retained"}));
        let text = redacted.to_string();
        assert!(!text.contains("secret"));
        assert!(!text.contains("alice"));
        assert!(!text.contains("trackingId"));
        assert!(!text.contains("deviceId"));
        assert!(text.contains("ref=main"));
        assert_eq!(redacted["unrelated"], "retained");
    }

    #[test]
    fn archive_and_multipart_use_same_opted_in_files() -> Result<(), String> {
        let bundle = BugReportBundle {
            id: "id".into(),
            metadata: json!({"id":"id"}),
            diagnostics: json!({}),
            transcript: None,
            summary: Some("summary".into()),
            crash_log: None,
        };
        let files = bundle.files()?;
        assert_eq!(
            files.iter().map(|file| file.0).collect::<Vec<_>>(),
            ["report.json", "diagnostics.json", "summary.md"]
        );
        assert_eq!(files[2].2, Cow::Borrowed(b"summary\n".as_slice()));
        let data =
            String::from_utf8(multipart(&files, "boundary")?).map_err(|error| error.to_string())?;
        assert!(data.contains("name=\"summary.md\"; filename=\"summary.md\"\r\nContent-Type: text/markdown\r\n\r\nsummary\n"));
        assert!(!data.contains("session.jsonl"));
        assert!(data.ends_with("--boundary--\r\n"));
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let archive = directory.path().join("report.zip");
        write_report_zip(&archive, &files)?;
        assert!(write_report_zip(&archive, &files).is_err());
        let extracted = crate::utils::zip::extract_zip_archive(&archive, directory.path())?;
        assert_eq!(extracted.len(), 3);
        assert!(!directory.path().join("session.jsonl").exists());
        Ok(())
    }

    #[test]
    fn summary_keeps_latest_turn_and_explicit_user_hint() {
        let messages = [
            json!({"role":"user","content":"earlier messages"}),
            json!({"role":"user","content":"latest request"}),
        ];
        let prompt = summary_prompt(&messages, 1, Some("  process hung  "));
        assert!(prompt.contains("only the last 1 of 2 messages"));
        assert!(!prompt.contains("earlier messages"));
        assert!(prompt.contains("[User]: latest request"));
        assert!(prompt.contains("<user-report>\nprocess hung\n</user-report>"));
    }

    #[tokio::test]
    async fn bug_report_loopback_e2e_uploads_redacted_multipart_bundle() -> Result<(), String> {
        use crate::core::agent_session::{
            AgentSessionConfig, InMemoryModelRegistry, InMemorySettings, NoopExtensionRunner,
            StaticAuth, TailCompactor,
        };
        use crate::core::agent_session_services::DiskSession;
        use crate::core::session_manager::SessionManager;
        use pi_rs_agent::{Agent, AgentInitialState, AgentOptions};
        use pi_rs_ai::faux::{FauxOptions, faux_provider};
        use std::sync::Arc;

        let faux = faux_provider(FauxOptions::default());
        let model = faux.get_model(None).ok_or("Missing faux model")?.clone();
        let models = Models::default();
        models.set_provider(faux.provider);
        let agent = Agent::new(AgentOptions {
            initial_state: AgentInitialState {
                model: Some(model.clone()),
                ..AgentInitialState::default()
            },
            stream_fn: Some(pi_rs_agent::models_stream_fn(models)),
            ..AgentOptions::default()
        })
        .map_err(|error| error.to_string())?;
        let manager = Arc::new(DiskSession::new(SessionManager::in_memory()));
        {
            let mut disk = manager.lock();
            disk.append_message(json!({
                "role": "user",
                "content": "please check",
                "timestamp": 0
            }))?;
            disk.append_message(json!({
                "role": "assistant",
                "content": [{"type": "text", "text": "failed"}],
                "stopReason": "error",
                "errorMessage": "Authorization: Bearer sk-live-supersecret999999 failed at https://alice:topsecret@api.example.test/v1?api_key=querysecret",
                "timestamp": 1
            }))?;
        }
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

        let settings = SettingsManager::in_memory(
            json!({
                "apiKey": "global-secret-key",
                "trackingId": "tracking-123",
                "deviceId": "device-456",
                "safeSetting": "keep-me",
                "customToken": "project-secret-token"
            }),
            crate::core::settings_manager::CreateOptions::default(),
        );
        let bundle = build_bug_report(
            &session,
            &settings,
            &BugReportOptions {
                hint: Some("upload failure".into()),
                include_session: true,
                include_summary: false,
            },
            None,
        )?;

        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        std::fs::write(
            dir.path().join("auth.json"),
            br#"{"radius":{"type":"api_key","key":"bug-e2e-token"}}"#,
        )
        .map_err(|error| error.to_string())?;

        let (gateway, rx) =
            crate::modes::interactive::utilities::delivery::tests::spawn_loopback_json_server(
                "200 OK",
                json!({"ok": true, "bug_report": {"id": "br-e2e-99"}}),
            )
            .await?;
        let signal = CancellationToken::new();
        let report_id = upload_bug_report_with_options(
            &session,
            &bundle,
            dir.path(),
            &BugReportUploadOptions {
                consent: true,
                gateway_override: Some(gateway),
                offline_override: Some(false),
            },
            &signal,
        )
        .await?;
        assert_eq!(report_id.as_deref(), Some("br-e2e-99"));

        let captured = rx.await.map_err(|error| error.to_string())?;
        assert_eq!(captured.method, "POST");
        assert_eq!(captured.target, "/v1/bug-reports");
        assert_eq!(
            captured.headers.get("authorization").map(String::as_str),
            Some("Bearer bug-e2e-token")
        );
        let content_type = captured
            .headers
            .get("content-type")
            .ok_or("missing content-type")?;
        let boundary = content_type
            .split("boundary=")
            .nth(1)
            .ok_or("missing multipart boundary")?;
        let body = String::from_utf8(captured.body).map_err(|error| error.to_string())?;

        let delimiter = format!("--{boundary}");
        let mut parts = std::collections::HashMap::new();
        for segment in body.split(&delimiter) {
            let trimmed = segment.strip_prefix("\r\n").unwrap_or(segment);
            if trimmed.is_empty() || trimmed.starts_with("--") {
                continue;
            }
            if let Some((headers, content)) = trimmed.split_once("\r\n\r\n")
                && let Some(name) = headers
                    .split("name=\"")
                    .nth(1)
                    .and_then(|tail| tail.split('"').next())
            {
                let content = content.strip_suffix("\r\n").unwrap_or(content);
                parts.insert(name.to_owned(), content.to_owned());
            }
        }
        assert!(parts.contains_key("report.json"));
        assert!(parts.contains_key("diagnostics.json"));
        assert!(parts.contains_key("session.jsonl"));
        assert!(!parts.contains_key("summary.md"));

        let report_json: Value =
            serde_json::from_str(&parts["report.json"]).map_err(|error| error.to_string())?;
        assert_eq!(report_json["hint"], "upload failure");
        assert_eq!(report_json["settings"]["global"]["safeSetting"], "keep-me");
        assert_eq!(
            report_json["settings"]["global"]["apiKey"],
            Value::from("<redacted>")
        );
        assert_eq!(
            report_json["settings"]["global"]["customToken"],
            Value::from("<redacted>")
        );
        assert!(
            report_json["settings"]["global"]
                .get("trackingId")
                .is_none()
        );
        assert!(report_json["settings"]["global"].get("deviceId").is_none());

        let diagnostics_json: Value =
            serde_json::from_str(&parts["diagnostics.json"]).map_err(|error| error.to_string())?;
        let diagnostics_text = diagnostics_json.to_string();
        assert!(!diagnostics_text.contains("sk-live-supersecret999999"));
        assert!(!diagnostics_text.contains("topsecret"));
        assert!(!diagnostics_text.contains("querysecret"));
        assert!(!diagnostics_text.contains("global-secret-key"));
        assert!(!diagnostics_text.contains("project-secret-token"));
        assert!(diagnostics_text.contains("<redacted>"));

        let has_delivery_entry = manager.lock().get_entries().iter().any(|entry| {
            matches!(
                entry,
                crate::core::session_manager::SessionEntry::Custom { custom_type, .. }
                if custom_type == REPORT_ENTRY
            )
        });
        assert!(has_delivery_entry);
        Ok(())
    }
}
