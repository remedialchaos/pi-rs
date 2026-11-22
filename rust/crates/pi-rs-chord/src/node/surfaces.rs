//! Content-addressed native surface artifacts and live script manifests.
use super::{
    http::{HttpContribution, HttpError, HttpResponse, RouteOptions, json_response},
    plugins::digest,
};
use crate::{
    Error, Result,
    context::Context,
    host::{Facet, Members, ServeOptions, define_facet},
    json::JsonValue,
    state::Source,
    surfaces::{ScriptEntry, ScriptSet},
};
use axum::body::Body;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
};
#[derive(Clone, Debug)]
pub struct ServedFile {
    pub mime: String,
    pub bytes: Vec<u8>,
}
impl ServedFile {
    pub fn response(&self) -> std::result::Result<HttpResponse, HttpError> {
        ::http::Response::builder()
            .header("Content-Type", &self.mime)
            .header("Content-Length", self.bytes.len())
            .header("Cache-Control", "private, max-age=31536000, immutable")
            .body(Body::from(self.bytes.clone()))
            .map_err(|_| HttpError::new(500, "invalid artifact response"))
    }
}
#[derive(Clone, Debug)]
pub struct SurfaceArtifact {
    pub name: String,
    pub bytes: Vec<u8>,
    pub css: Option<Vec<u8>>,
    pub assets: BTreeMap<String, ServedFile>,
}
#[derive(Clone, Debug)]
pub enum SurfaceInput {
    Built(SurfaceArtifact),
    Failed { name: String, error: String },
}
struct Published {
    current: BTreeMap<String, SurfaceArtifact>,
    sets: VecDeque<BTreeMap<String, ServedFile>>,
    manifest: ScriptSet,
    readables: BTreeMap<String, Source>,
}
#[derive(Clone)]
pub struct Surface {
    name: String,
    prefix: String,
    base: String,
    state: Arc<Mutex<Published>>,
}
impl Surface {
    /// `base_bytes` is an already-built Rust WASM web module or a native terminal artifact.
    pub fn new(name: &str, base_bytes: Vec<u8>) -> Result<Self> {
        if !matches!(name, "web" | "tui") {
            return Err(Error::Invalid("unknown surface".into()));
        }
        let prefix = format!("/_chord/{name}/");
        let base = digest(&base_bytes);
        let base_path = format!("{prefix}base.{base}.wasm");
        let state = Published {
            current: BTreeMap::new(),
            sets: VecDeque::from([BTreeMap::from([(
                base_path,
                ServedFile {
                    mime: "application/wasm".into(),
                    bytes: base_bytes,
                },
            )])]),
            manifest: ScriptSet {
                base: base.clone(),
                combined: String::new(),
                plugins: vec![],
            },
            readables: BTreeMap::new(),
        };
        let surface = Self {
            name: name.into(),
            prefix,
            base,
            state: Arc::new(Mutex::new(state)),
        };
        surface.publish(vec![], &Context::background())?;
        Ok(surface)
    }
    pub fn manifest(&self) -> ScriptSet {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .manifest
            .clone()
    }
    pub fn file(&self, path: &str) -> Option<ServedFile> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sets
            .iter()
            .rev()
            .find_map(|files| files.get(path).cloned())
    }
    pub fn publish(
        &self,
        inputs: Vec<SurfaceInput>,
        context: &Context,
    ) -> Result<Vec<(String, String)>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = BTreeMap::new();
        let mut failures = Vec::new();
        for input in inputs {
            match input {
                SurfaceInput::Built(artifact) => {
                    crate::contracts::check_name(&artifact.name, "surface plugin")?;
                    for name in artifact.assets.keys() {
                        super::app::safe_relative(name)?;
                    }
                    if next.insert(artifact.name.clone(), artifact).is_some() {
                        return Err(Error::Invalid("duplicate surface plugin".into()));
                    }
                }
                SurfaceInput::Failed { name, error } => {
                    if let Some(previous) = state.current.get(&name) {
                        next.insert(name.clone(), previous.clone());
                    }
                    failures.push((name, error));
                }
            }
        }
        let mut files = BTreeMap::new();
        let mut entries = Vec::new();
        // Base files are retained independently of rolling plugin sets.
        for set in &state.sets {
            for (path, file) in set {
                if path.starts_with(&format!("{}base.", self.prefix)) {
                    files.insert(path.clone(), file.clone());
                }
            }
        }
        for (name, artifact) in &next {
            let hash = digest(&artifact.bytes);
            let stem = format!("{}plugin/{}.{}", self.prefix, name.replace('/', "~"), hash);
            let url = format!("{stem}.wasm");
            files.insert(
                url.clone(),
                ServedFile {
                    mime: "application/wasm".into(),
                    bytes: artifact.bytes.clone(),
                },
            );
            let css = artifact.css.as_ref().map(|css| {
                let path = format!("{stem}.{}.css", digest(css));
                files.insert(
                    path.clone(),
                    ServedFile {
                        mime: "text/css; charset=utf-8".into(),
                        bytes: css.clone(),
                    },
                );
                path
            });
            entries.push(ScriptEntry {
                name: name.clone(),
                digest: hash,
                url,
                css,
            });
            for (name, file) in &artifact.assets {
                files.insert(
                    format!("{}assets/{}/{name}", self.prefix, digest(&file.bytes)),
                    file.clone(),
                );
            }
        }
        let bytes = serde_json::to_vec(&entries)?;
        let combined = format!("{}plugins.{}.json", self.prefix, digest(&bytes));
        files.insert(
            combined.clone(),
            ServedFile {
                mime: "application/json".into(),
                bytes,
            },
        );
        let manifest = ScriptSet {
            base: self.base.clone(),
            combined,
            plugins: entries,
        };
        let value = JsonValue::from_serde(serde_json::to_value(&manifest)?);
        let readables = state.readables.values().cloned().collect::<Vec<_>>();
        state.current = next;
        state.manifest = manifest;
        state.sets.push_back(files);
        while state.sets.len() > 4 {
            state.sets.pop_front();
        }
        drop(state);
        for readable in readables {
            readable.set(value.clone(), context)?;
        }
        Ok(failures)
    }
    pub fn facet(&self) -> Result<Facet> {
        let contract = Arc::new(crate::surfaces::scripts_contract(&format!(
            "chord.{}.scripts",
            self.name
        ))?);
        let surface = self.clone();
        Ok(define_facet(move |host| {
            let surface = surface.clone();
            host.serve(
                contract.clone(),
                move |_, _, self_| {
                    let readable = self_.writable(JsonValue::from_serde(serde_json::to_value(
                        surface.manifest(),
                    )?))?;
                    let state = surface.state.clone();
                    let source = readable.clone();
                    let closing = state.clone();
                    let id = uuid::Uuid::new_v4().to_string();
                    self_.open(
                        move |_| {
                            Box::pin(async move {
                                state
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .readables
                                    .insert(id.clone(), source);
                                Ok(id)
                            })
                        },
                        move |id, _| {
                            Box::pin(async move {
                                closing
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .readables
                                    .remove(id.as_str());
                                Ok(())
                            })
                        },
                    );
                    let mut members = Members::variant("main");
                    members.state("set", readable)?;
                    Ok(members)
                },
                ServeOptions::default(),
            )?;
            Ok(())
        }))
    }
    pub fn contribution(&self, public: bool) -> std::result::Result<HttpContribution, HttpError> {
        let mut contribution = HttpContribution::new(format!("chord.surface.{}", self.name));
        let surface = self.clone();
        let mut route = RouteOptions::new(move |request| {
            let surface = surface.clone();
            Box::pin(async move {
                if request.path == format!("{}base", surface.prefix) {
                    return json_response(&serde_json::json!(surface.base));
                }
                if request.path == format!("{}start", surface.prefix) {
                    let manifest = surface.manifest();
                    return json_response(
                        &serde_json::json!({"base":manifest.base,"script":format!("{}base.{}.wasm",surface.prefix,surface.base),"combined":manifest.combined}),
                    );
                }
                surface
                    .file(&request.path)
                    .ok_or_else(|| HttpError::new(404, "surface file not found"))?
                    .response()
            })
        });
        route.public = public;
        contribution.route(&format!("GET {}*", self.prefix), route)?;
        Ok(contribution)
    }
}
