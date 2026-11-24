//! Artifacts (worker facet): registers the `p2-artifacts` Durable extension and formats new
//! open html page errors for `MessageContexts`.

use std::sync::Arc;

use pi_rs_chord::{
    Result,
    context::Context,
    contracts::Contract,
    host::{Facet, Host, define_facet},
    json::JsonValue,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::api::v1::{p2_contracts::lookup, worker::worker_of};

use super::{
    pages::{ErrorMark, EvalResult, LogEntry},
    store::{ArtifactEdit, ArtifactRead, ArtifactSummary, WriteInput},
    tool::{ArtifactBackend, BoxBackendFuture, create_artifact_extension},
};

struct ChordArtifactBackend {
    host: Host,
    session: String,
    contract: Arc<Contract>,
}

impl ChordArtifactBackend {
    async fn call_worker(&self, method: &str, args: Value, context: &Context) -> Result<Value> {
        let root = self.host.root();
        let held = root.hold(
            Arc::clone(&self.contract),
            JsonValue::from_serde(json!({ "session": self.session })),
        )?;
        held.wait(context).await?;
        let out = held
            .call(method, JsonValue::from_serde(args), context)
            .await?;
        out.to_serde()
    }
}

impl ArtifactBackend for ChordArtifactBackend {
    fn list<'a>(&'a self, context: &'a Context) -> BoxBackendFuture<'a, Vec<ArtifactSummary>> {
        Box::pin(async move {
            let value = self.call_worker("list", Value::Null, context).await?;
            Ok(serde_json::from_value(value)?)
        })
    }

    fn read<'a>(
        &'a self,
        id: &'a str,
        version: Option<i64>,
        context: &'a Context,
    ) -> BoxBackendFuture<'a, ArtifactRead> {
        Box::pin(async move {
            let mut args = json!({ "id": id });
            if let Some(version) = version {
                args["version"] = json!(version);
            }
            let value = self.call_worker("read", args, context).await?;
            Ok(serde_json::from_value(value)?)
        })
    }

    fn write<'a>(
        &'a self,
        input: WriteInput,
        context: &'a Context,
    ) -> BoxBackendFuture<'a, (String, i64)> {
        Box::pin(async move {
            #[derive(Deserialize)]
            struct Out {
                id: String,
                version: i64,
            }
            let value = self
                .call_worker("write", serde_json::to_value(input)?, context)
                .await?;
            let out: Out = serde_json::from_value(value)?;
            Ok((out.id, out.version))
        })
    }

    fn edit<'a>(
        &'a self,
        id: &'a str,
        edits: Vec<ArtifactEdit>,
        context: &'a Context,
    ) -> BoxBackendFuture<'a, i64> {
        Box::pin(async move {
            #[derive(Deserialize)]
            struct Out {
                version: i64,
            }
            let value = self
                .call_worker("edit", json!({ "id": id, "edits": edits }), context)
                .await?;
            let out: Out = serde_json::from_value(value)?;
            Ok(out.version)
        })
    }

    fn remove<'a>(&'a self, id: &'a str, context: &'a Context) -> BoxBackendFuture<'a, ()> {
        Box::pin(async move {
            self.call_worker("remove", json!({ "id": id }), context)
                .await?;
            Ok(())
        })
    }

    fn state<'a>(&'a self, id: &'a str, context: &'a Context) -> BoxBackendFuture<'a, Value> {
        Box::pin(async move {
            let value = self
                .call_worker("readState", json!({ "id": id }), context)
                .await?;
            Ok(value.get("state").cloned().unwrap_or(Value::Null))
        })
    }

    fn set_state<'a>(
        &'a self,
        id: &'a str,
        state: Value,
        context: &'a Context,
    ) -> BoxBackendFuture<'a, ()> {
        Box::pin(async move {
            self.call_worker("setState", json!({ "id": id, "state": state }), context)
                .await?;
            Ok(())
        })
    }

    fn errors<'a>(
        &'a self,
        id: &'a str,
        context: &'a Context,
    ) -> BoxBackendFuture<'a, Vec<LogEntry>> {
        Box::pin(async move {
            let value = self
                .call_worker("errors", json!({ "id": id }), context)
                .await?;
            Ok(serde_json::from_value(value)?)
        })
    }

    fn logs<'a>(
        &'a self,
        id: &'a str,
        since: Option<i64>,
        context: &'a Context,
    ) -> BoxBackendFuture<'a, (Vec<LogEntry>, i64)> {
        Box::pin(async move {
            #[derive(Deserialize)]
            struct Out {
                logs: Vec<LogEntry>,
                next: i64,
            }
            let mut args = json!({ "id": id });
            if let Some(since) = since {
                args["since"] = json!(since);
            }
            let value = self.call_worker("logs", args, context).await?;
            let out: Out = serde_json::from_value(value)?;
            Ok((out.logs, out.next))
        })
    }

    fn run<'a>(
        &'a self,
        id: &'a str,
        code: &'a str,
        context: &'a Context,
    ) -> BoxBackendFuture<'a, EvalResult> {
        Box::pin(async move {
            let value = self
                .call_worker("eval", json!({ "id": id, "code": code }), context)
                .await?;
            Ok(serde_json::from_value(value)?)
        })
    }
}

/// Carried message context for new open html artifact errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CarriedArtifactErrors {
    pub text: String,
    pub note: String,
    pub marks: Vec<ErrorMark>,
}

/// Fetches uncarried errors of open html pages for `session` and formats the `<artifact-errors>`
/// block to attach to the next user message.
pub async fn collect_new_errors(
    host: &Host,
    session: &str,
    context: &Context,
) -> Result<Option<CarriedArtifactErrors>> {
    #[derive(Deserialize)]
    struct Item {
        id: String,
        title: String,
        text: String,
    }
    #[derive(Deserialize)]
    struct Out {
        errors: Vec<Item>,
        marks: Vec<ErrorMark>,
    }
    let contract = Arc::new(lookup("p2.artifacts")?);
    let root = host.root();
    let held = root.hold(
        contract,
        JsonValue::from_serde(json!({ "session": session })),
    )?;
    held.wait(context).await?;
    let raw = held
        .call("newErrors", JsonValue::Null, context)
        .await?
        .to_serde()?;
    let out: Out = serde_json::from_value(raw)?;
    if out.errors.is_empty() {
        return Ok(None);
    }
    let mut lines = vec![
        "<artifact-errors>".to_owned(),
        "New errors of html artifacts open on the person's devices (artifact_html_errors lists them; the text is the page's):".to_owned(),
    ];
    for err in &out.errors {
        lines.push(format!("- {} ({}): {}", err.id, err.title, err.text));
    }
    lines.push("</artifact-errors>".to_owned());
    let count = out.errors.len();
    let note = format!("{count} page error{}", if count == 1 { "" } else { "s" });
    Ok(Some(CarriedArtifactErrors {
        text: lines.join("\n"),
        note,
        marks: out.marks,
    }))
}

/// Marks previously collected artifact errors as carried.
pub async fn mark_errors_carried(
    host: &Host,
    session: &str,
    marks: &[ErrorMark],
    context: &Context,
) -> Result<()> {
    let contract = Arc::new(lookup("p2.artifacts")?);
    let root = host.root();
    let held = root.hold(
        contract,
        JsonValue::from_serde(json!({ "session": session })),
    )?;
    held.wait(context).await?;
    held.call(
        "carried",
        JsonValue::from_serde(json!({ "marks": marks })),
        context,
    )
    .await?;
    Ok(())
}

/// Builds the worker facet for the `artifacts` plugin.
pub fn worker_facet() -> Result<Facet> {
    let contract = Arc::new(lookup("p2.artifacts")?);
    Ok(define_facet(move |host| {
        let host_handle = host.host();
        if let Ok(worker) = worker_of(&host_handle)
            && let Ok(session) = worker.init.session()
        {
            let backend: Arc<dyn ArtifactBackend> = Arc::new(ChordArtifactBackend {
                host: host_handle,
                session: session.id,
                contract: Arc::clone(&contract),
            });
            worker
                .registries
                .extensions
                .install(create_artifact_extension(backend))?;
        }
        Ok(())
    }))
}
