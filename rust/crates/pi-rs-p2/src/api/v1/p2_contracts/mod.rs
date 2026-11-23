//! Shared p2 plugin wire declarations, recorded from upstream Chord's contractData.
//! Schema indexes are local to each declaration. Chord snapshots and fingerprints
//! each version as it is built, including edits that leave the wire shape identical.

use indexmap::IndexMap;
use pi_rs_chord::{
    Error, Result,
    contracts::{Contract, HostKind, Member, MemberKind, read_params, schema},
    json::JsonValue,
};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Route {
    Server,
    Session,
}
impl Route {
    fn host(&self) -> Result<HostKind> {
        let (name, params) = match self {
            Self::Server => ("server", vec![]),
            Self::Session => ("session", vec![("id".into(), schema::str())]),
        };
        HostKind::new(name, read_params(&params, name)?)
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum WireMember {
    State(usize),
    Event(usize),
    Method(usize, usize),
    Keyed(usize, Option<Route>),
    Queries(usize, Vec<(String, usize)>),
}
type Members = Vec<(String, WireMember)>;
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum RemovedKind {
    State,
    Methods,
}
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Edit {
    Put(String, String, WireMember, bool),
    Remove(String, String, RemovedKind),
    Args(String, usize),
    Result(String, usize),
    Method(String, usize, usize),
}
#[derive(Deserialize)]
struct Declaration {
    version: u64,
    params: Vec<(String, usize)>,
    route: Option<Route>,
    variants: Vec<(String, Members)>,
    history: Vec<Vec<Edit>>,
    admission: Option<usize>,
    schemas: Vec<Value>,
}
/// Builds the named declaration and its upstream compatibility history.
pub fn lookup(name: &str) -> Result<Contract> {
    let source = match name {
        "p2.access" => include_str!("p2.access.json"),
        "p2.agent-locations" => include_str!("p2.agent-locations.json"),
        "p2.agent-terminals" => include_str!("p2.agent-terminals.json"),
        "p2.archived-starts" => include_str!("p2.archived-starts.json"),
        "p2.artifact-page" => include_str!("p2.artifact-page.json"),
        "p2.artifacts" => include_str!("p2.artifacts.json"),
        "p2.attachments" => include_str!("p2.attachments.json"),
        "p2.backup" => include_str!("p2.backup.json"),
        "p2.backup-remote" => include_str!("p2.backup-remote.json"),
        "p2.catalog" => include_str!("p2.catalog.json"),
        "p2.chat" => include_str!("p2.chat.json"),
        "p2.chat-costs" => include_str!("p2.chat-costs.json"),
        "p2.chat-images" => include_str!("p2.chat-images.json"),
        "p2.chat-mode" => include_str!("p2.chat-mode.json"),
        "p2.chat-mode-setting" => include_str!("p2.chat-mode-setting.json"),
        "p2.child-store" => include_str!("p2.child-store.json"),
        "p2.codemode-runtime" => include_str!("p2.codemode-runtime.json"),
        "p2.conversation" => include_str!("p2.conversation.json"),
        "p2.conversation-broker" => include_str!("p2.conversation-broker.json"),
        "p2.conversations" => include_str!("p2.conversations.json"),
        "p2.device-access" => include_str!("p2.device-access.json"),
        "p2.devices" => include_str!("p2.devices.json"),
        "p2.discord" => include_str!("p2.discord.json"),
        "p2.discord-ingress" => include_str!("p2.discord-ingress.json"),
        "p2.discord-store" => include_str!("p2.discord-store.json"),
        "p2.downloads" => include_str!("p2.downloads.json"),
        "p2.draft-store" => include_str!("p2.draft-store.json"),
        "p2.drafts" => include_str!("p2.drafts.json"),
        "p2.env-service" => include_str!("p2.env-service.json"),
        "p2.envs" => include_str!("p2.envs.json"),
        "p2.familiar" => include_str!("p2.familiar.json"),
        "p2.familiar-store" => include_str!("p2.familiar-store.json"),
        "p2.file-index" => include_str!("p2.file-index.json"),
        "p2.file-index-cache" => include_str!("p2.file-index-cache.json"),
        "p2.forward-store" => include_str!("p2.forward-store.json"),
        "p2.guest-presence" => include_str!("p2.guest-presence.json"),
        "p2.harness-host" => include_str!("p2.harness-host.json"),
        "p2.hosts" => include_str!("p2.hosts.json"),
        "p2.log-notices" => include_str!("p2.log-notices.json"),
        "p2.login-service" => include_str!("p2.login-service.json"),
        "p2.logs" => include_str!("p2.logs.json"),
        "p2.mcp" => include_str!("p2.mcp.json"),
        "p2.mcp.manager" => include_str!("p2.mcp.manager.json"),
        "p2.model-config" => include_str!("p2.model-config.json"),
        "p2.models" => include_str!("p2.models.json"),
        "p2.peer-conversation" => include_str!("p2.peer-conversation.json"),
        "p2.plugin-reload" => include_str!("p2.plugin-reload.json"),
        "p2.ports" => include_str!("p2.ports.json"),
        "p2.powershell" => include_str!("p2.powershell.json"),
        "p2.project-service" => include_str!("p2.project-service.json"),
        "p2.projects" => include_str!("p2.projects.json"),
        "p2.prompt-resources" => include_str!("p2.prompt-resources.json"),
        "p2.provider-login" => include_str!("p2.provider-login.json"),
        "p2.proxy-marker" => include_str!("p2.proxy-marker.json"),
        "p2.radius-service" => include_str!("p2.radius-service.json"),
        "p2.rollup-store" => include_str!("p2.rollup-store.json"),
        "p2.run-control" => include_str!("p2.run-control.json"),
        "p2.sandbox-secrets" => include_str!("p2.sandbox-secrets.json"),
        "p2.scope-stage" => include_str!("p2.scope-stage.json"),
        "p2.search" => include_str!("p2.search.json"),
        "p2.search-feed" => include_str!("p2.search-feed.json"),
        "p2.search-index" => include_str!("p2.search-index.json"),
        "p2.search-index-scan" => include_str!("p2.search-index-scan.json"),
        "p2.sent-files" => include_str!("p2.sent-files.json"),
        "p2.server" => include_str!("p2.server.json"),
        "p2.server-name" => include_str!("p2.server-name.json"),
        "p2.server-name-settings" => include_str!("p2.server-name-settings.json"),
        "p2.session-files" => include_str!("p2.session-files.json"),
        "p2.session-index" => include_str!("p2.session-index.json"),
        "p2.session-lock" => include_str!("p2.session-lock.json"),
        "p2.session-locks" => include_str!("p2.session-locks.json"),
        "p2.session-metrics" => include_str!("p2.session-metrics.json"),
        "p2.session-moves" => include_str!("p2.session-moves.json"),
        "p2.session-operations" => include_str!("p2.session-operations.json"),
        "p2.session-reads" => include_str!("p2.session-reads.json"),
        "p2.session-status" => include_str!("p2.session-status.json"),
        "p2.session-transfer" => include_str!("p2.session-transfer.json"),
        "p2.sessions" => include_str!("p2.sessions.json"),
        "p2.settings" => include_str!("p2.settings.json"),
        "p2.share-presence" => include_str!("p2.share-presence.json"),
        "p2.shares" => include_str!("p2.shares.json"),
        "p2.sharing" => include_str!("p2.sharing.json"),
        "p2.shell-commands" => include_str!("p2.shell-commands.json"),
        "p2.sidebar-rollups" => include_str!("p2.sidebar-rollups.json"),
        "p2.sidebar-settings" => include_str!("p2.sidebar-settings.json"),
        "p2.sidebar-settings-store" => include_str!("p2.sidebar-settings-store.json"),
        "p2.status-report" => include_str!("p2.status-report.json"),
        "p2.status-store" => include_str!("p2.status-store.json"),
        "p2.subagent-control" => include_str!("p2.subagent-control.json"),
        "p2.subagents" => include_str!("p2.subagents.json"),
        "p2.terminal-service" => include_str!("p2.terminal-service.json"),
        "p2.terminals" => include_str!("p2.terminals.json"),
        "p2.traces" => include_str!("p2.traces.json"),
        "p2.tree" => include_str!("p2.tree.json"),
        "p2.tui-preferences" => include_str!("p2.tui-preferences.json"),
        "p2.views" => include_str!("p2.views.json"),
        "p2.voice-mode" => include_str!("p2.voice-mode.json"),
        "p2.voice-service" => include_str!("p2.voice-service.json"),
        "p2.voice-service-store" => include_str!("p2.voice-service-store.json"),
        "p2.worker-boundary" => include_str!("p2.worker-boundary.json"),
        "p2.worker-control" => include_str!("p2.worker-control.json"),
        "p2.worker-models" => include_str!("p2.worker-models.json"),
        "p2.workspace" => include_str!("p2.workspace.json"),
        "p2.workspace-backend" => include_str!("p2.workspace-backend.json"),
        "p2.workspace-transfer" => include_str!("p2.workspace-transfer.json"),
        "p2.workspace-writes" => include_str!("p2.workspace-writes.json"),
        _ => return Err(Error::Invalid(format!("unknown p2 contract: {name}"))),
    };
    let declaration: Declaration =
        serde_json::from_str(source).map_err(|e| Error::Invalid(format!("{name}: {e}")))?;
    let schema = |i: usize| {
        declaration
            .schemas
            .get(i)
            .cloned()
            .map(JsonValue::from_serde)
            .ok_or_else(|| Error::Invalid(format!("{name}: unknown schema {i}")))
    };
    let member = |wire: WireMember| -> Result<Member> {
        Ok(match wire {
            WireMember::State(i) => Member::State(schema(i)?),
            WireMember::Event(i) => Member::Event(schema(i)?),
            WireMember::Method(a, r) => Member::Method {
                args: schema(a)?,
                result: schema(r)?,
            },
            WireMember::Keyed(i, key) => Member::Keyed {
                schema: schema(i)?,
                key: key.map(|k| k.host()).transpose()?,
            },
            WireMember::Queries(i, args) => Member::Queries {
                item: schema(i)?,
                args: args
                    .into_iter()
                    .map(|(k, i)| Ok((k, schema(i)?)))
                    .collect::<Result<_>>()?,
            },
        })
    };
    let params = declaration
        .params
        .into_iter()
        .map(|(k, i)| Ok((k, schema(i)?)))
        .collect::<Result<_>>()?;
    let mut variants = IndexMap::new();
    for (v, members) in declaration.variants {
        variants.insert(
            v,
            members
                .into_iter()
                .map(|(k, m)| Ok((k, member(m)?)))
                .collect::<Result<_>>()?,
        );
    }
    let mut contract = if let Some(route) = declaration.route {
        let host = route.host()?;
        Contract::remote(
            name,
            declaration.version,
            params,
            variants,
            Arc::new(move |params| {
                let fields = match route {
                    Route::Server => IndexMap::new(),
                    Route::Session => IndexMap::from([(
                        "id".into(),
                        params
                            .get("session")
                            .cloned()
                            .ok_or_else(|| Error::Invalid("missing session".into()))?,
                    )]),
                };
                host.address(JsonValue::Object(fields))
            }),
        )?
    } else {
        Contract::local(
            name,
            declaration.version,
            params,
            variants
                .shift_remove("members")
                .ok_or_else(|| Error::Invalid(format!("{name}: missing members")))?,
        )?
    };
    for edits in declaration.history {
        contract = contract.evolve(|editor| {
            for edit in edits {
                match edit {
                    Edit::Put(v, k, m, add) => editor.put(&v, &k, member(m)?, add)?,
                    Edit::Remove(v, k, kind) => editor.remove(
                        &v,
                        &k,
                        match kind {
                            RemovedKind::State => MemberKind::State,
                            RemovedKind::Methods => MemberKind::Method,
                        },
                    )?,
                    Edit::Args(k, i) => editor.method_args(&k, schema(i)?)?,
                    Edit::Result(k, i) => editor.method_result(&k, schema(i)?)?,
                    Edit::Method(k, a, r) => editor.method(&k, schema(a)?, schema(r)?)?,
                };
            }
            Ok(())
        })?;
    }
    if let Some(i) = declaration.admission {
        contract = contract.with_admission(schema(i)?)?;
    }
    Ok(contract)
}
