//! Version checking, port of `packages/coding-agent/src/utils/version-check.ts`.
//!
//! Network access is injectable: every public fetch helper has a `_with` variant
//! that accepts a fetch function, so tests can exercise retry/offline logic without
//! touching the network. The plain helpers use `reqwest`.

use std::future::Future;
use std::time::Duration;

use node_semver::Version;
use serde::Serialize;
use serde_json::Value;

/// URL used by the automatic version check.
pub const LATEST_VERSION_URL: &str = "https://pi.dev/api/latest-version";
/// Default request timeout in milliseconds.
pub const DEFAULT_VERSION_CHECK_TIMEOUT_MS: u64 = 10_000;
/// Statuses that `fetchWithRetry` treats as transient.
pub const RETRYABLE_STATUSES: [u16; 7] = [408, 425, 429, 500, 502, 503, 504];

/// Metadata returned by the version check API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LatestPiRelease {
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One cause in a network error chain (aggregated causes in upstream).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorCause {
    pub code: Option<String>,
    pub message: Option<String>,
}

/// Options for a version check request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VersionCheckOptions {
    pub timeout_ms: Option<u64>,
    pub retry: bool,
}

/// A request passed to the injectable fetch function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchRequest {
    pub url: String,
    pub user_agent: String,
    pub accept: String,
}

/// A response returned from the injectable fetch function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchResponse {
    pub ok: bool,
    pub status: u16,
    pub body: String,
}

/// Stringify an error chain the way `formatVersionCheckError` does: append the
/// deduplicated error codes in insertion order, else the first cause message.
pub fn format_version_check_error(root_message: &str, causes: &[ErrorCause]) -> String {
    let mut codes: Vec<&str> = Vec::new();
    for cause in causes {
        if let Some(code) = &cause.code
            && !codes.contains(&code.as_str())
        {
            codes.push(code.as_str());
        }
    }
    if !codes.is_empty() {
        return format!("{root_message} ({})", codes.join(", "));
    }
    if let Some(message) = causes
        .iter()
        .find_map(|cause| cause.message.as_deref().filter(|m| !m.is_empty()))
    {
        return format!("{root_message} (cause: {message})");
    }
    root_message.to_string()
}

/// Strip the leading `v`/`=` decorations accepted by npm's `semver.valid`.
fn normalize_semver(value: &str) -> &str {
    let mut value = value.trim();
    loop {
        match value.strip_prefix('v').or_else(|| value.strip_prefix('=')) {
            Some(rest) => value = rest.trim_start(),
            None => return value,
        }
    }
}

/// Compare two versions by semver precedence. `None` means at least one input is
/// not a valid semver version.
pub fn compare_package_versions(left_version: &str, right_version: &str) -> Option<i8> {
    let left = Version::parse(normalize_semver(left_version)).ok()?;
    let right = Version::parse(normalize_semver(right_version)).ok()?;
    Some(match left.cmp(&right) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    })
}

/// True when `candidate_version` is newer than `current_version`. Invalid semver
/// versions fall back to trimmed string inequality.
pub fn is_newer_package_version(candidate_version: &str, current_version: &str) -> bool {
    match compare_package_versions(candidate_version, current_version) {
        Some(order) => order > 0,
        None => candidate_version.trim() != current_version.trim(),
    }
}

/// Parse the JSON body of a version check response.
pub fn parse_release_json(body: &str) -> Option<LatestPiRelease> {
    let value: Value = serde_json::from_str(body).ok()?;
    let object = value.as_object()?;
    let version = object.get("version")?.as_str()?.trim();
    if version.is_empty() {
        return None;
    }
    let non_empty_trimmed = |key: &str| -> Option<String> {
        object
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    Some(LatestPiRelease {
        version: version.to_string(),
        package_name: non_empty_trimmed("packageName"),
        note: non_empty_trimmed("note"),
    })
}

/// True when an environment flag is present and non-empty (JavaScript truthiness).
fn env_flag_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

fn user_agent(current_version: &str) -> String {
    // Matches the `detailed` user agent shape: `pi/<version> (...)`.
    format!("pi/{current_version} (rust; +https://pi.dev)")
}

async fn default_fetch(request: FetchRequest, timeout_ms: u64) -> Result<FetchResponse, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .build()
        .map_err(|error| error.to_string())?;
    let response = client
        .get(&request.url)
        .header("User-Agent", request.user_agent)
        .header("accept", request.accept)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status().as_u16();
    let ok = response.status().is_success();
    let body = response.text().await.map_err(|error| error.to_string())?;
    Ok(FetchResponse { ok, status, body })
}

/// Fetch the latest release with an injectable fetch function.
///
/// `fetch` is called up to `1 + max_retries` times; retryable statuses and
/// transport failures trigger a retry. Offline mode returns `Ok(None)` without
/// calling `fetch`.
pub async fn get_latest_pi_release_with<F, Fut>(
    current_version: &str,
    options: VersionCheckOptions,
    fetch: F,
) -> Result<Option<LatestPiRelease>, String>
where
    F: Fn(FetchRequest, u64) -> Fut,
    Fut: Future<Output = Result<FetchResponse, String>>,
{
    if env_flag_set("PI_OFFLINE") {
        return Ok(None);
    }

    let timeout_ms = options
        .timeout_ms
        .unwrap_or(DEFAULT_VERSION_CHECK_TIMEOUT_MS);
    let request = FetchRequest {
        url: LATEST_VERSION_URL.to_string(),
        user_agent: user_agent(current_version),
        accept: "application/json".to_string(),
    };
    let max_retries = if options.retry { 2 } else { 0 };
    let mut last_error = None;

    for _ in 0..=max_retries {
        match fetch(request.clone(), timeout_ms).await {
            Ok(response) => {
                if !response.ok {
                    if RETRYABLE_STATUSES.contains(&response.status) {
                        continue;
                    }
                    return Ok(None);
                }
                return Ok(parse_release_json(&response.body));
            }
            Err(error) => {
                last_error = Some(error);
            }
        }
    }

    match last_error {
        Some(error) => Err(error),
        None => Ok(None),
    }
}

/// Fetch the latest release over HTTP.
pub async fn get_latest_pi_release(
    current_version: &str,
    options: VersionCheckOptions,
) -> Result<Option<LatestPiRelease>, String> {
    get_latest_pi_release_with(current_version, options, |request, timeout_ms| {
        default_fetch(request, timeout_ms)
    })
    .await
}

/// Fetch just the latest version string.
pub async fn get_latest_pi_version_with<F, Fut>(
    current_version: &str,
    options: VersionCheckOptions,
    fetch: F,
) -> Result<Option<String>, String>
where
    F: Fn(FetchRequest, u64) -> Fut,
    Fut: Future<Output = Result<FetchResponse, String>>,
{
    Ok(get_latest_pi_release_with(current_version, options, fetch)
        .await?
        .map(|release| release.version))
}

/// Fetch just the latest version string over HTTP.
pub async fn get_latest_pi_version(
    current_version: &str,
    options: VersionCheckOptions,
) -> Result<Option<String>, String> {
    get_latest_pi_version_with(current_version, options, |request, timeout_ms| {
        default_fetch(request, timeout_ms)
    })
    .await
}

/// Automatic version check: skips when `PI_SKIP_VERSION_CHECK` is set, makes one
/// request (no retries), and swallows all errors.
pub async fn check_for_new_pi_version_with<F, Fut>(
    current_version: &str,
    fetch: F,
) -> Option<LatestPiRelease>
where
    F: Fn(FetchRequest, u64) -> Fut,
    Fut: Future<Output = Result<FetchResponse, String>>,
{
    if env_flag_set("PI_SKIP_VERSION_CHECK") {
        return None;
    }
    let release =
        get_latest_pi_release_with(current_version, VersionCheckOptions::default(), fetch)
            .await
            .ok()??;
    if is_newer_package_version(&release.version, current_version) {
        Some(release)
    } else {
        None
    }
}

/// Automatic version check over HTTP.
pub async fn check_for_new_pi_version(current_version: &str) -> Option<LatestPiRelease> {
    check_for_new_pi_version_with(current_version, |request, timeout_ms| {
        default_fetch(request, timeout_ms)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_package_versions() {
        assert_eq!(compare_package_versions("0.70.6", "0.70.5"), Some(1));
        assert_eq!(compare_package_versions("0.70.5", "0.70.5"), Some(0));
        assert_eq!(compare_package_versions("0.70.4", "0.70.5"), Some(-1));
        assert_eq!(
            compare_package_versions("5.0.0-beta.20", "5.0.0-beta.9"),
            Some(1)
        );
        assert_eq!(compare_package_versions(" v1.2.3 ", "1.2.3"), Some(0));
        assert_eq!(compare_package_versions("1.2.3+build.7", "1.2.3"), Some(0));
    }

    #[test]
    fn invalid_versions_are_not_comparable() {
        assert_eq!(compare_package_versions("latest", "1.2.3"), None);
        assert_eq!(compare_package_versions("1.2", "1.2.3"), None);
        assert_eq!(compare_package_versions("", "1.2.3"), None);
    }

    #[test]
    fn newer_version_checks() {
        assert!(!is_newer_package_version("0.70.5", "0.70.5"));
        assert!(is_newer_package_version("0.70.6", "0.70.5"));
        assert!(is_newer_package_version("latest", "1.2.3"));
        assert!(!is_newer_package_version(" 1.2.3 ", "1.2.3"));
    }

    #[test]
    fn formats_nested_network_error_details() {
        let causes = [
            ErrorCause {
                code: Some("ETIMEDOUT".to_string()),
                message: Some("connect timeout".to_string()),
            },
            ErrorCause {
                code: Some("ENETUNREACH".to_string()),
                message: Some("network unreachable".to_string()),
            },
        ];
        assert_eq!(
            format_version_check_error("fetch failed", &causes),
            "fetch failed (ETIMEDOUT, ENETUNREACH)"
        );
    }

    #[test]
    fn deduplicates_codes_and_uses_cause_message_fallback() {
        let causes = [
            ErrorCause {
                code: Some("ECONNRESET".to_string()),
                message: None,
            },
            ErrorCause {
                code: Some("ECONNRESET".to_string()),
                message: None,
            },
        ];
        assert_eq!(
            format_version_check_error("boom", &causes),
            "boom (ECONNRESET)"
        );

        let causes = [ErrorCause {
            code: None,
            message: Some("root cause".to_string()),
        }];
        assert_eq!(
            format_version_check_error("boom", &causes),
            "boom (cause: root cause)"
        );
        assert_eq!(format_version_check_error("boom", &[]), "boom");
    }

    #[test]
    fn parses_release_json_fields() {
        assert_eq!(
            parse_release_json(
                r#"{"version":"1.2.4","packageName":" @new-scope/pi ","note":" **Read this** "}"#
            ),
            Some(LatestPiRelease {
                version: "1.2.4".to_string(),
                package_name: Some("@new-scope/pi".to_string()),
                note: Some("**Read this**".to_string()),
            })
        );
        assert_eq!(
            parse_release_json(r#"{"version":"1.2.4"}"#),
            Some(LatestPiRelease {
                version: "1.2.4".to_string(),
                package_name: None,
                note: None,
            })
        );
        assert_eq!(parse_release_json(r#"{"version":"  "}"#), None);
        assert_eq!(parse_release_json("not json"), None);
    }

    #[tokio::test]
    async fn injectable_fetch_retries_transient_errors() {
        use std::sync::{Arc, Mutex};

        let calls = Arc::new(Mutex::new(0));
        let calls_for_fetch = Arc::clone(&calls);
        let fetch = move |_request: FetchRequest, _timeout: u64| {
            let calls = Arc::clone(&calls_for_fetch);
            async move {
                let mut guard = calls.lock().expect("lock must be held");
                *guard += 1;
                let attempt = *guard;
                drop(guard);
                if attempt < 3 {
                    Err("fetch failed".to_string())
                } else {
                    Ok(FetchResponse {
                        ok: true,
                        status: 200,
                        body: r#"{"version":"1.2.4"}"#.to_string(),
                    })
                }
            }
        };

        let release = get_latest_pi_release_with(
            "1.2.3",
            VersionCheckOptions {
                retry: true,
                timeout_ms: None,
            },
            fetch,
        )
        .await
        .expect("fetch should succeed")
        .expect("release should be present");
        assert_eq!(release.version, "1.2.4");
        assert_eq!(*calls.lock().expect("lock must be held"), 3);
    }

    #[tokio::test]
    async fn injectable_fetch_makes_one_request_without_retry() {
        use std::sync::{Arc, Mutex};

        let calls = Arc::new(Mutex::new(0));
        let calls_for_fetch = Arc::clone(&calls);
        let fetch = move |_request: FetchRequest, _timeout: u64| {
            let calls = Arc::clone(&calls_for_fetch);
            async move {
                *calls.lock().expect("lock must be held") += 1;
                Err("fetch failed".to_string())
            }
        };

        let result =
            get_latest_pi_release_with("1.2.3", VersionCheckOptions::default(), fetch).await;
        assert!(result.is_err());
        assert_eq!(*calls.lock().expect("lock must be held"), 1);
    }
}
