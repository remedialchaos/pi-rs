//! `projects` built-in plugin: owner project groupings, ordered places, session assignments,
//! `projects.json` one-time import, and `p2.project-service` / `p2.projects`.

pub mod config;
pub mod place;

use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use pi_rs_chord::{
    Error, Result,
    data::ChordError,
    documents::Definition,
    host::{Members, ServeOptions, define_facet},
    json::JsonValue,
    state::Source,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::v1::{FacetKind, PluginRegistry, Principal, p2_contracts::lookup};

use config::{ArchivedProject, Project, ProjectPlace, load_projects, parse_places};
use place::project_at;

const GAP: i64 = 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectRow {
    name: String,
    places: Vec<ProjectPlace>,
    position: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    archived_at: Option<u64>,
    #[serde(default)]
    archived_sessions: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectsState {
    imported: bool,
    projects: BTreeMap<String, ProjectRow>,
    members: BTreeMap<String, String>,
}

impl ProjectsState {
    fn last_position(&self) -> i64 {
        self.projects
            .values()
            .map(|row| row.position)
            .max()
            .unwrap_or(0)
    }

    fn live_projects(&self) -> Vec<Project> {
        let mut live: Vec<&ProjectRow> = self
            .projects
            .values()
            .filter(|row| row.archived_at.is_none())
            .collect();
        live.sort_by(|a, b| {
            a.position
                .cmp(&b.position)
                .then_with(|| a.name.cmp(&b.name))
        });
        live.into_iter()
            .map(|row| Project {
                name: row.name.clone(),
                places: row.places.clone(),
            })
            .collect()
    }

    fn archived_projects(&self) -> Vec<ArchivedProject> {
        let mut archived: Vec<ArchivedProject> = self
            .projects
            .values()
            .filter_map(|row| {
                row.archived_at.map(|archived_at| ArchivedProject {
                    name: row.name.clone(),
                    places: row.places.clone(),
                    archived_at,
                })
            })
            .collect();
        archived.sort_by_key(|a| std::cmp::Reverse(a.archived_at));
        archived
    }

    fn members_of(&self, name: &str) -> Vec<String> {
        self.members
            .iter()
            .filter(|(_, project)| project.as_str() == name)
            .map(|(session, _)| session.clone())
            .collect()
    }

    fn named(&self, name: &str) -> Result<&ProjectRow> {
        self.projects
            .get(name)
            .ok_or_else(|| Error::Invalid(format!("No project \"{name}\"")))
    }

    fn live(&self, name: &str) -> Result<&ProjectRow> {
        let row = self.named(name)?;
        if row.archived_at.is_some() {
            return Err(Error::Invalid(format!("Project \"{name}\" is archived")));
        }
        Ok(row)
    }

    fn free_name(&self, name: &str, except: Option<&str>) -> Result<String> {
        let trimmed = name.trim().to_owned();
        parse_places(&trimmed, &json!([{ "path": "/" }]))?;
        if except != Some(trimmed.as_str()) && self.projects.contains_key(&trimmed) {
            return Err(Error::Invalid(format!(
                "A project \"{trimmed}\" exists already"
            )));
        }
        Ok(trimmed)
    }

    fn checked_place(name: &str, place: &ProjectPlace) -> Result<ProjectPlace> {
        let value = match &place.env {
            None => json!([{ "path": place.path.trim() }]),
            Some(env) => json!([{ "env": env, "path": place.path.trim() }]),
        };
        let mut parsed = parse_places(name, &value)?;
        parsed
            .pop()
            .ok_or_else(|| Error::Invalid(format!("Project {name}: invalid place")))
    }

    fn import_from_disk(&mut self, data_dir: &Path, catalog_sessions: &[Value]) {
        if self.imported {
            return;
        }
        let file = load_projects(data_dir).unwrap_or_default();
        let mut position = self.last_position();
        for project in &file {
            if self.projects.contains_key(&project.name) {
                continue;
            }
            position += GAP;
            self.projects.insert(
                project.name.clone(),
                ProjectRow {
                    name: project.name.clone(),
                    places: project.places.clone(),
                    position,
                    archived_at: None,
                    archived_sessions: Vec::new(),
                },
            );
        }
        let known: HashSet<&str> = file.iter().map(|p| p.name.as_str()).collect();
        for session in catalog_sessions {
            let Some(id) = session.get("id").and_then(Value::as_str) else {
                continue;
            };
            let Some(cwd) = session.get("cwd").and_then(Value::as_str) else {
                continue;
            };
            let env = session.get("env").and_then(Value::as_str);
            if let Some(found) = project_at(&file, env, cwd, None)
                && known.contains(found.name.as_str())
                && !self.members.contains_key(id)
            {
                self.members.insert(id.to_owned(), found.name.clone());
            }
        }
        self.imported = true;
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn local_hostname() -> String {
    #[cfg(unix)]
    {
        nix::unistd::gethostname()
            .ok()
            .and_then(|s| s.into_string().ok())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "local".into())
    }
    #[cfg(not(unix))]
    {
        std::env::var("COMPUTERNAME")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "local".into())
    }
}

#[derive(Deserialize)]
struct AssignArgs {
    session: String,
    project: Option<String>,
}

#[derive(Deserialize)]
struct CopyArgs {
    source: String,
    copy: String,
}

#[derive(Deserialize)]
struct CreateArgs {
    name: String,
    place: ProjectPlace,
}

#[derive(Deserialize)]
struct RenameArgs {
    name: String,
    to: String,
}

#[derive(Deserialize)]
struct LocateArgs {
    name: String,
    place: ProjectPlace,
}

#[derive(Deserialize)]
struct MoveArgs {
    name: String,
    before: Option<String>,
}

#[derive(Deserialize)]
struct NameArgs {
    name: String,
}

fn publish_sources(
    state: &ProjectsState,
    projects_src: &Source,
    archived_src: &Source,
    sessions_src: &Source,
    ctx: &pi_rs_chord::context::Context,
) -> Result<()> {
    projects_src.set(
        JsonValue::from_serde(serde_json::to_value(state.live_projects())?),
        ctx,
    )?;
    archived_src.set(
        JsonValue::from_serde(serde_json::to_value(state.archived_projects())?),
        ctx,
    )?;
    sessions_src.set(
        JsonValue::from_serde(serde_json::to_value(&state.members)?),
        ctx,
    )?;
    Ok(())
}

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    let service_contract = Arc::new(lookup("p2.project-service")?);
    let projects_contract = Arc::new(lookup("p2.projects")?);
    let server_contract = Arc::new(lookup("p2.server")?);
    let catalog_contract = Arc::new(lookup("p2.catalog")?);
    let ops_contract = Arc::new(lookup("p2.session-operations")?);

    registry.register(
        "projects",
        FacetKind::Server,
        define_facet(move |host| {
            let service_for_remote = service_contract.clone();
            let server_ref = server_contract.clone();
            let catalog_ref = catalog_contract.clone();
            let ops_ref = ops_contract.clone();

            host.serve(
                service_contract.clone(),
                move |_, _, object| {
                    let doc_def = Definition::new(
                        "p2.projects-state",
                        JsonValue::from_serde(json!({})),
                        1,
                        || {
                            Ok(JsonValue::from_serde(serde_json::to_value(
                                ProjectsState::default(),
                            )?))
                        },
                    )?;
                    let doc_def = if object.store().is_some() {
                        doc_def.persisted(true, |_, _, _| true)
                    } else {
                        doc_def
                    };
                    let doc = object.state(Arc::new(doc_def))?;
                    let initial_state: ProjectsState = doc
                        .value()
                        .ok()
                        .and_then(|v| v.to_serde().ok())
                        .and_then(|v| serde_json::from_value(v).ok())
                        .unwrap_or_default();

                    let mem = Arc::new(Mutex::new(initial_state.clone()));
                    let projects_src = object.writable(JsonValue::from_serde(
                        serde_json::to_value(initial_state.live_projects())?,
                    ))?;
                    let archived_src = object.writable(JsonValue::from_serde(
                        serde_json::to_value(initial_state.archived_projects())?,
                    ))?;
                    let sessions_src = object.writable(JsonValue::from_serde(
                        serde_json::to_value(&initial_state.members)?,
                    ))?;

                    let server = object
                        .host()
                        .served_contract("p2.server")
                        .map(|_| object.get(server_ref.clone(), JsonValue::from_serde(json!({}))))
                        .transpose()?;
                    let catalog = object
                        .host()
                        .served_contract("p2.catalog")
                        .map(|_| object.get(catalog_ref.clone(), JsonValue::from_serde(json!({}))))
                        .transpose()?;
                    let ops = object
                        .host()
                        .served_contract("p2.session-operations")
                        .map(|_| object.get(ops_ref.clone(), JsonValue::from_serde(json!({}))))
                        .transpose()?;

                    if let (Some(server_held), Some(catalog_held)) =
                        (server.clone(), catalog.clone())
                    {
                        let import_mem = mem.clone();
                        let import_doc = doc.clone();
                        let p_src = projects_src.clone();
                        let a_src = archived_src.clone();
                        let s_src = sessions_src.clone();
                        object.on_live(move |ctx| {
                            let server_held = server_held.clone();
                            let catalog_held = catalog_held.clone();
                            let import_mem = import_mem.clone();
                            let import_doc = import_doc.clone();
                            let p_src = p_src.clone();
                            let a_src = a_src.clone();
                            let s_src = s_src.clone();
                            Box::pin(async move {
                                let info =
                                    server_held.member("info")?.value()?.to_json().to_serde()?;
                                let Some(data_dir) = info.get("dataDir").and_then(Value::as_str)
                                else {
                                    return Ok(());
                                };
                                let sessions_val = catalog_held
                                    .member("sessions")
                                    .and_then(|s| s.value())
                                    .ok()
                                    .and_then(|v| v.to_json().to_serde().ok())
                                    .and_then(|v| v.as_array().cloned())
                                    .unwrap_or_default();
                                let snapshot = {
                                    let mut guard =
                                        import_mem.lock().unwrap_or_else(|p| p.into_inner());
                                    if guard.imported {
                                        return Ok(());
                                    }
                                    guard.import_from_disk(Path::new(data_dir), &sessions_val);
                                    guard.clone()
                                };
                                let serialized =
                                    JsonValue::from_serde(serde_json::to_value(&snapshot)?);
                                let _ = import_doc
                                    .change(&ctx.context, |draft| {
                                        Box::pin(async move {
                                            draft.set(&[], serialized)?;
                                            Ok(())
                                        })
                                    })
                                    .await;
                                publish_sources(&snapshot, &p_src, &a_src, &s_src, &ctx.context)?;
                                Ok(())
                            })
                        });
                    }

                    let mut members = Members::new();
                    members.state("projects", projects_src.clone())?;
                    members.state("archived", archived_src.clone())?;
                    members.state("sessions", sessions_src.clone())?;

                    macro_rules! mutate_method {
                        ($name:expr, $arg_ty:ty, $body:expr) => {{
                            let m = mem.clone();
                            let d = doc.clone();
                            let ps = projects_src.clone();
                            let asrc = archived_src.clone();
                            let ss = sessions_src.clone();
                            members.method($name, move |args, ctx| {
                                let m = m.clone();
                                let d = d.clone();
                                let ps = ps.clone();
                                let asrc = asrc.clone();
                                let ss = ss.clone();
                                Box::pin(async move {
                                    let parsed: $arg_ty = serde_json::from_value(args.to_serde()?)?;
                                    let next_snapshot = {
                                        let mut guard = m.lock().unwrap_or_else(|p| p.into_inner());
                                        let mut draft_state = guard.clone();
                                        let apply: fn(&mut ProjectsState, $arg_ty) -> Result<()> =
                                            $body;
                                        apply(&mut draft_state, parsed)?;
                                        *guard = draft_state.clone();
                                        draft_state
                                    };
                                    let serialized = JsonValue::from_serde(serde_json::to_value(
                                        &next_snapshot,
                                    )?);
                                    let _ = d
                                        .change(&ctx.context, |draft| {
                                            Box::pin(async move {
                                                draft.set(&[], serialized)?;
                                                Ok(())
                                            })
                                        })
                                        .await;
                                    publish_sources(&next_snapshot, &ps, &asrc, &ss, &ctx.context)?;
                                    Ok(JsonValue::Null)
                                })
                            })?;
                        }};
                    }

                    mutate_method!("assign", AssignArgs, |state, args| {
                        match args.project {
                            None => {
                                state.members.remove(&args.session);
                            }
                            Some(project) => {
                                state.live(&project)?;
                                state.members.insert(args.session, project);
                            }
                        }
                        Ok(())
                    });

                    mutate_method!("copy", CopyArgs, |state, args| {
                        if let Some(project) = state.members.get(&args.source).cloned() {
                            state.members.insert(args.copy, project);
                        }
                        Ok(())
                    });

                    mutate_method!("create", CreateArgs, |state, args| {
                        let free = state.free_name(&args.name, None)?;
                        let checked = ProjectsState::checked_place(&free, &args.place)?;
                        let position = state.last_position() + GAP;
                        state.projects.insert(
                            free.clone(),
                            ProjectRow {
                                name: free,
                                places: vec![checked],
                                position,
                                archived_at: None,
                                archived_sessions: Vec::new(),
                            },
                        );
                        Ok(())
                    });

                    mutate_method!("rename", RenameArgs, |state, args| {
                        let row = state.named(&args.name)?.clone();
                        let free = state.free_name(&args.to, Some(&args.name))?;
                        if free == args.name {
                            return Ok(());
                        }
                        state.projects.remove(&args.name);
                        let mut updated = row;
                        updated.name = free.clone();
                        state.projects.insert(free.clone(), updated);
                        for session in state.members_of(&args.name) {
                            state.members.insert(session, free.clone());
                        }
                        Ok(())
                    });

                    mutate_method!("locate", LocateArgs, |state, args| {
                        let row = state.named(&args.name)?.clone();
                        let checked = ProjectsState::checked_place(&args.name, &args.place)?;
                        let mut places = vec![checked.clone()];
                        places.extend(
                            row.places
                                .into_iter()
                                .filter(|each| each.env != checked.env),
                        );
                        if let Some(entry) = state.projects.get_mut(&args.name) {
                            entry.places = places;
                        }
                        Ok(())
                    });

                    mutate_method!("move", MoveArgs, |state, args| {
                        let row = state.live(&args.name)?.clone();
                        if args.before.as_deref() == Some(args.name.as_str()) {
                            return Ok(());
                        }
                        let mut order: Vec<ProjectRow> = state
                            .projects
                            .values()
                            .filter(|each| each.name != args.name)
                            .cloned()
                            .collect();
                        order.sort_by(|a, b| {
                            a.position
                                .cmp(&b.position)
                                .then_with(|| a.name.cmp(&b.name))
                        });
                        let at = match &args.before {
                            None => order.len(),
                            Some(before) => order
                                .iter()
                                .position(|each| each.name == *before)
                                .ok_or_else(|| {
                                    Error::Invalid(format!("No project \"{before}\""))
                                })?,
                        };
                        let after = at
                            .checked_sub(1)
                            .and_then(|idx| order.get(idx))
                            .map(|r| r.position);
                        let next = order.get(at).map(|r| r.position);
                        let position = match (after, next) {
                            (None, n) => n.unwrap_or(GAP) - GAP,
                            (Some(a), None) => a + GAP,
                            (Some(a), Some(n)) => (a + n).div_euclid(2),
                        };
                        if Some(position) == after || Some(position) == next {
                            order.insert(at, row);
                            for (index, mut each) in order.into_iter().enumerate() {
                                each.position = ((index + 1) as i64) * GAP;
                                state.projects.insert(each.name.clone(), each);
                            }
                        } else if let Some(entry) = state.projects.get_mut(&args.name) {
                            entry.position = position;
                        }
                        Ok(())
                    });

                    mutate_method!("remove", NameArgs, |state, args| {
                        state.named(&args.name)?;
                        state.projects.remove(&args.name);
                        for session in state.members_of(&args.name) {
                            state.members.remove(&session);
                        }
                        Ok(())
                    });

                    // Archive & restore coordinate with p2.catalog & p2.session-operations when present.
                    {
                        let m = mem.clone();
                        let d = doc.clone();
                        let ps = projects_src.clone();
                        let asrc = archived_src.clone();
                        let ss = sessions_src.clone();
                        let cat = catalog.clone();
                        let op = ops.clone();
                        members.method("archive", move |args, ctx| {
                            let m = m.clone();
                            let d = d.clone();
                            let ps = ps.clone();
                            let asrc = asrc.clone();
                            let ss = ss.clone();
                            let cat = cat.clone();
                            let op = op.clone();
                            Box::pin(async move {
                                let args: NameArgs = serde_json::from_value(args.to_serde()?)?;
                                let member_set: HashSet<String> = {
                                    let guard = m.lock().unwrap_or_else(|p| p.into_inner());
                                    guard.live(&args.name)?;
                                    guard.members_of(&args.name).into_iter().collect()
                                };
                                let mut archived_ids = Vec::new();
                                if let (Some(cat_held), Some(op_held)) = (cat, op) {
                                    let rows = cat_held
                                        .member("sessions")?
                                        .value()?
                                        .to_json()
                                        .to_serde()?;
                                    if let Some(list) = rows.as_array() {
                                        for row in list {
                                            let id = row.get("id").and_then(Value::as_str);
                                            let live_row =
                                                row.get("archivedAt").is_none_or(Value::is_null);
                                            if let Some(id) = id
                                                && live_row
                                                && member_set.contains(id)
                                            {
                                                op_held
                                                    .call(
                                                        "archive",
                                                        JsonValue::from_serde(json!({ "id": id })),
                                                        &ctx.context,
                                                    )
                                                    .await?;
                                                archived_ids.push(id.to_owned());
                                            }
                                        }
                                    }
                                }
                                let snapshot = {
                                    let mut guard = m.lock().unwrap_or_else(|p| p.into_inner());
                                    guard.live(&args.name)?;
                                    if let Some(entry) = guard.projects.get_mut(&args.name) {
                                        entry.archived_at = Some(now_ms());
                                        entry.archived_sessions = archived_ids;
                                    }
                                    guard.clone()
                                };
                                let serialized =
                                    JsonValue::from_serde(serde_json::to_value(&snapshot)?);
                                let _ = d
                                    .change(&ctx.context, |draft| {
                                        Box::pin(async move {
                                            draft.set(&[], serialized)?;
                                            Ok(())
                                        })
                                    })
                                    .await;
                                publish_sources(&snapshot, &ps, &asrc, &ss, &ctx.context)?;
                                Ok(JsonValue::Null)
                            })
                        })?;
                    }

                    {
                        let m = mem.clone();
                        let d = doc.clone();
                        let ps = projects_src.clone();
                        let asrc = archived_src.clone();
                        let ss = sessions_src.clone();
                        let cat = catalog.clone();
                        let op = ops.clone();
                        members.method("restore", move |args, ctx| {
                            let m = m.clone();
                            let d = d.clone();
                            let ps = ps.clone();
                            let asrc = asrc.clone();
                            let ss = ss.clone();
                            let cat = cat.clone();
                            let op = op.clone();
                            Box::pin(async move {
                                let args: NameArgs = serde_json::from_value(args.to_serde()?)?;
                                let archived_set: HashSet<String> = {
                                    let guard = m.lock().unwrap_or_else(|p| p.into_inner());
                                    let row = guard.named(&args.name)?;
                                    if row.archived_at.is_none() {
                                        return Ok(JsonValue::Null);
                                    }
                                    row.archived_sessions.iter().cloned().collect()
                                };
                                if let (Some(cat_held), Some(op_held)) = (cat, op) {
                                    let rows = cat_held
                                        .member("sessions")?
                                        .value()?
                                        .to_json()
                                        .to_serde()?;
                                    if let Some(list) = rows.as_array() {
                                        for row in list {
                                            let id = row.get("id").and_then(Value::as_str);
                                            let is_archived =
                                                row.get("archivedAt").is_some_and(|v| !v.is_null());
                                            if let Some(id) = id
                                                && is_archived
                                                && archived_set.contains(id)
                                            {
                                                op_held
                                                    .call(
                                                        "restore",
                                                        JsonValue::from_serde(json!({ "id": id })),
                                                        &ctx.context,
                                                    )
                                                    .await?;
                                            }
                                        }
                                    }
                                }
                                let snapshot = {
                                    let mut guard = m.lock().unwrap_or_else(|p| p.into_inner());
                                    let next_pos = guard.last_position() + GAP;
                                    if let Some(entry) = guard.projects.get_mut(&args.name) {
                                        entry.archived_at = None;
                                        entry.archived_sessions.clear();
                                        entry.position = next_pos;
                                    }
                                    guard.clone()
                                };
                                let serialized =
                                    JsonValue::from_serde(serde_json::to_value(&snapshot)?);
                                let _ = d
                                    .change(&ctx.context, |draft| {
                                        Box::pin(async move {
                                            draft.set(&[], serialized)?;
                                            Ok(())
                                        })
                                    })
                                    .await;
                                publish_sources(&snapshot, &ps, &asrc, &ss, &ctx.context)?;
                                Ok(JsonValue::Null)
                            })
                        })?;
                    }

                    Ok(members)
                },
                ServeOptions::default(),
            )?;

            host.serve(
                projects_contract.clone(),
                move |_, principal, object| {
                    let parsed = principal
                        .and_then(|v| Principal::deserialize(v).ok())
                        .ok_or_else(|| ChordError::new("forbidden", "projects are owner-only"))?;
                    if !parsed.is_owner() {
                        return Err(ChordError::new("forbidden", "projects are owner-only").into());
                    }
                    let service =
                        object.get(service_for_remote.clone(), JsonValue::from_serde(json!({})))?;
                    let label = local_hostname();
                    let mut members = Members::variant("owner");
                    members.state(
                        "projects",
                        object.follow_source(service.member("projects")?)?,
                    )?;
                    members.state(
                        "archived",
                        object.follow_source(service.member("archived")?)?,
                    )?;
                    members.state(
                        "local",
                        object.writable(JsonValue::from_serde(json!(label)))?,
                    )?;
                    members.state(
                        "sessions",
                        object.follow_source(service.member("sessions")?)?,
                    )?;
                    for method_name in [
                        "assign", "create", "rename", "locate", "move", "archive", "restore",
                        "remove",
                    ] {
                        let svc = service.clone();
                        members.method(method_name, move |args, ctx| {
                            let svc = svc.clone();
                            Box::pin(async move { svc.call(method_name, args, &ctx.context).await })
                        })?;
                    }
                    Ok(members)
                },
                ServeOptions::default(),
            )
        }),
    )
}
