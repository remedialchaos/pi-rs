//! Server-side discovery, digest indexing, and authenticated HTTP delivery of TUI WIT components.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::{Arc, RwLock},
};

use axum::body::Body;
use http::{HeaderValue, StatusCode, header};
use pi_rs_chord::{
    Result,
    node::{
        http::{
            HttpContribution, HttpError, HttpRequest, HttpResponse, HttpResult, RouteOptions,
            text_response,
        },
        plugins::{PluginRoot, digest, discover_plugins},
    },
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::runtime::{host_dirs, principals::Principal};

/// Maximum size of a single TUI WIT component served over `/__p2_tui`.
pub const MAX_TUI_BUNDLE: usize = 4 * 1024 * 1024;

/// Maximum combined size of all TUI WIT components in a single manifest.
pub const MAX_TOTAL_TUI_BUNDLES: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TuiBundleEntry {
    pub name: String,
    pub digest: String,
    pub url: String,
    pub bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TuiManifest {
    pub version: String,
    pub plugins: Vec<TuiBundleEntry>,
}

#[derive(Clone, Debug)]
pub struct TuiBundleIndex {
    manifest: TuiManifest,
    manifest_json: Vec<u8>,
    assets: BTreeMap<String, Arc<[u8]>>,
}

pub type SharedTuiBundles = Arc<RwLock<TuiBundleIndex>>;

impl TuiBundleIndex {
    pub fn build(data_dir: &Path, reserved: &BTreeSet<String>) -> Result<Self> {
        let root = PluginRoot {
            path: host_dirs::user_plugins_directory(data_dir),
            required: false,
        };
        let found = discover_plugins(&[root], &["tui"])?;
        for problem in found.problems {
            eprintln!(
                "p2 tui bundles: {}: {}",
                problem.path.display(),
                problem.error
            );
        }

        let mut entries = Vec::new();
        let mut assets = BTreeMap::new();
        let mut total_bytes = 0usize;

        for plugin in found.plugins {
            let Some(path) = plugin.facets.get("tui") else {
                continue;
            };
            if reserved.contains(&plugin.name) {
                eprintln!(
                    "p2 tui bundles: {}: plugin {} is reserved by a built-in",
                    path.display(),
                    plugin.name
                );
                continue;
            }
            let bytes = match std::fs::read(path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    eprintln!("p2 tui bundles: {}: {error}", path.display());
                    continue;
                }
            };
            if bytes.len() > MAX_TUI_BUNDLE {
                eprintln!(
                    "p2 tui bundles: {}: component exceeds {} byte limit ({} bytes)",
                    path.display(),
                    MAX_TUI_BUNDLE,
                    bytes.len()
                );
                continue;
            }
            if total_bytes.saturating_add(bytes.len()) > MAX_TOTAL_TUI_BUNDLES {
                eprintln!(
                    "p2 tui bundles: {}: total bundle budget ({} bytes) exceeded",
                    path.display(),
                    MAX_TOTAL_TUI_BUNDLES
                );
                continue;
            }
            total_bytes += bytes.len();
            let sha = digest(&bytes);
            entries.push(TuiBundleEntry {
                name: plugin.name,
                url: format!("/__p2_tui/{sha}.wasm"),
                bytes: bytes.len(),
                digest: sha.clone(),
            });
            assets.insert(sha, Arc::from(bytes));
        }

        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let version = manifest_version(&entries);
        let manifest = TuiManifest {
            version,
            plugins: entries,
        };
        let manifest_json = serde_json::to_vec(&manifest)
            .map_err(|e| pi_rs_chord::Error::Invalid(e.to_string()))?;
        Ok(Self {
            manifest,
            manifest_json,
            assets,
        })
    }

    pub fn manifest(&self) -> &TuiManifest {
        &self.manifest
    }

    pub fn asset(&self, sha: &str) -> Option<&Arc<[u8]>> {
        self.assets.get(sha)
    }
}

fn manifest_version(entries: &[TuiBundleEntry]) -> String {
    if entries.is_empty() {
        return "empty".to_owned();
    }
    let mut hasher = Sha256::new();
    for entry in entries {
        hasher.update(entry.name.as_bytes());
        hasher.update([0]);
        hasher.update(entry.digest.as_bytes());
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

fn require_viewer(request: &HttpRequest) -> HttpResult<()> {
    let Some(value) = request.principal.as_ref() else {
        return Err(HttpError::new(401, "Unauthorized"));
    };
    match serde_json::from_value::<Principal>(value.clone()) {
        Ok(Principal::Owner { .. } | Principal::Guest { .. }) => Ok(()),
        _ => Err(HttpError::new(403, "Forbidden")),
    }
}

fn is_valid_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub(crate) fn add_routes(
    routes: &mut HttpContribution,
    bundles: SharedTuiBundles,
) -> HttpResult<()> {
    let get_bundles = bundles.clone();
    routes.route(
        "GET /__p2_tui",
        RouteOptions::new(move |request| {
            let bundles = get_bundles.clone();
            Box::pin(async move { serve_manifest(&request, &bundles, false) })
        }),
    )?;

    let head_bundles = bundles.clone();
    routes.route(
        "HEAD /__p2_tui",
        RouteOptions::new(move |request| {
            let bundles = head_bundles.clone();
            Box::pin(async move { serve_manifest(&request, &bundles, true) })
        }),
    )?;

    routes.route(
        "GET /__p2_tui/:asset",
        RouteOptions::new(move |request| {
            let bundles = bundles.clone();
            Box::pin(async move { serve_asset(&request, &bundles) })
        }),
    )?;

    Ok(())
}

fn serve_manifest(
    request: &HttpRequest,
    bundles: &SharedTuiBundles,
    head_only: bool,
) -> HttpResult<HttpResponse> {
    require_viewer(request)?;
    let json = {
        let guard = bundles.read().unwrap_or_else(|e| e.into_inner());
        guard.manifest_json.clone()
    };
    let len = json.len();
    let body = if head_only {
        Body::empty()
    } else {
        Body::from(json)
    };
    let mut response = HttpResponse::new(body);
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    Ok(response)
}

fn serve_asset(request: &HttpRequest, bundles: &SharedTuiBundles) -> HttpResult<HttpResponse> {
    require_viewer(request)?;
    let Some(asset) = request.params.get("asset") else {
        return text_response(404, "Not found");
    };
    let Some(sha) = asset.strip_suffix(".wasm") else {
        return text_response(404, "Not found");
    };
    if !is_valid_sha256_hex(sha) {
        return text_response(404, "Not found");
    }
    let bytes = {
        let guard = bundles.read().unwrap_or_else(|e| e.into_inner());
        guard.asset(sha).cloned()
    };
    let Some(bytes) = bytes else {
        return text_response(404, "Not found");
    };
    let len = bytes.len();
    let mut response = HttpResponse::new(Body::from(bytes.to_vec()));
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/wasm"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    Ok(response)
}
