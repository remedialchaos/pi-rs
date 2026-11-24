//! `/tree` plugin: session conversation tree, branch switching, labels, and read-only guard.

pub mod branch_target;
pub mod contract;
pub mod toc;

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use pi_rs_chord::{
    Error, Result,
    context::Context,
    contracts::Contract,
    data::ChordError,
    delta::Segment,
    host::{Ctx, Facet, Members, ServeOptions, define_facet},
    json::JsonValue,
};
use pi_rs_durable::{
    ConversationId, ConversationOwnership, ConversationRecord, EntryId, EntryQuery, EntryRecord,
    ROOT_CONVERSATION_ID, ScanOrder, Session, documents::resolve_address, harness::AGENT_DOC,
    session::Error as DurableError,
};
use serde_json::{Value, json};

use crate::{
    api::v1::{FacetKind, PluginRegistry, p2_contracts::lookup, worker::worker_of},
    runtime::{
        main_conversation::{
            MainBranch, MainState, is_top_level, main_conversation, main_doc, main_state,
            set_main_state, top_level_conversations, top_level_records,
        },
        principals::Principal,
    },
};

use contract::{PREVIEW_LENGTH, Tree, TreeBranch, TreeNode, TreeRole, session_tree_contract};

const NOT_CURRENT: &str =
    "This is an earlier branch of the session: switch to it with /tree to continue it";
const PAGE: usize = 500;

fn is_owner(principal: Option<&Value>) -> bool {
    principal
        .and_then(|v| serde_json::from_value::<Principal>(v.clone()).ok())
        .is_some_and(|p| p.is_owner())
}

fn one_line(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    let mut units = 0usize;
    for ch in flat.chars() {
        let u = ch.len_utf16();
        if units + u > PREVIEW_LENGTH {
            break;
        }
        units += u;
        out.push(ch);
    }
    out
}

fn role_of(entry: &EntryRecord) -> Option<TreeRole> {
    if entry.kind == "pi.assistant" {
        return Some(TreeRole::Assistant);
    }
    let first = entry.model.as_ref()?.first()?;
    if first.get("role").and_then(Value::as_str) == Some("user") && entry.head.is_none() {
        Some(TreeRole::User)
    } else {
        None
    }
}

fn user_text(entry: &EntryRecord) -> String {
    if let Some(text) = entry
        .data
        .as_ref()
        .and_then(|d| d.get("text"))
        .and_then(Value::as_str)
    {
        return text.to_string();
    }
    let Some(first) = entry.model.as_ref().and_then(|m| m.first()) else {
        return String::new();
    };
    if first.get("role").and_then(Value::as_str) != Some("user") {
        return String::new();
    }
    match first.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn assistant_text(entry: &EntryRecord) -> String {
    let Some(first) = entry.model.as_ref().and_then(|m| m.first()) else {
        return String::new();
    };
    let Some(parts) = first.get("content").and_then(Value::as_array) else {
        return String::new();
    };
    parts
        .iter()
        .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|p| p.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

fn preview_of(entry: &EntryRecord) -> Option<(TreeRole, String)> {
    let role = role_of(entry)?;
    let raw = match role {
        TreeRole::User => user_text(entry),
        TreeRole::Assistant => assistant_text(entry),
    };
    let text = one_line(&raw);
    (!text.is_empty()).then_some((role, text))
}

async fn fork_label(
    session: &Session,
    record: &ConversationRecord,
    context: &Context,
) -> Result<Option<String>> {
    if let Some(parent) = &record.parent {
        let at = parent.at;
        let point = session
            .commit(async move |tx| tx.entry(at).await, context)
            .await
            .map_err(|e| Error::Invalid(e.to_string()))?;
        if let Some((_, text)) = point.as_ref().and_then(preview_of) {
            return Ok(Some(text));
        }
    }
    let min_entry_id = Some(record.parent.as_ref().map_or(1, |p| p.at + 1));
    let mut cursor = None;
    loop {
        let page = session
            .scan_entries(
                &EntryQuery {
                    conversation_id: record.id,
                    min_entry_id,
                    max_entry_id: None,
                    order: Some(ScanOrder::Ascending),
                },
                PAGE,
                cursor.as_ref(),
                context,
            )
            .await
            .map_err(|e| Error::Invalid(e.to_string()))?;
        if let Some((_, text)) = page
            .items
            .iter()
            .filter_map(preview_of)
            .find(|(role, _)| *role == TreeRole::User)
        {
            return Ok(Some(text));
        }
        if page.next.is_none() {
            break;
        }
        cursor = page.next;
    }
    Ok(None)
}

pub async fn fill_labels(session: &Session, context: &Context) -> Result<()> {
    let state = main_state(session, context)
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let unlabeled: HashSet<ConversationId> = state
        .branches
        .iter()
        .filter(|b| b.label.is_none() && b.conversation != ROOT_CONVERSATION_ID)
        .map(|b| b.conversation)
        .collect();
    if unlabeled.is_empty() {
        return Ok(());
    }
    let records = top_level_records(session, context)
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let mut labels = HashMap::new();
    for record in &records {
        if !unlabeled.contains(&record.id) {
            continue;
        }
        if let Some(label) = fork_label(session, record, context).await? {
            labels.insert(record.id, label);
        }
    }
    if labels.is_empty() {
        return Ok(());
    }
    let mut next_state = state;
    for branch in &mut next_state.branches {
        if branch.label.is_none()
            && let Some(label) = labels.get(&branch.conversation)
        {
            branch.label = Some(label.clone());
        }
    }
    set_main_state(session, &next_state, context)
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;
    Ok(())
}

pub async fn report_tree(
    session: &Session,
    sessions_contract: &Arc<Contract>,
    ctx: &Ctx,
) -> Result<()> {
    let state = main_state(session, &ctx.context)
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let records = top_level_records(session, &ctx.context)
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let parents: HashMap<ConversationId, ConversationId> = records
        .into_iter()
        .filter_map(|r| r.parent.map(|p| (r.id, p.conversation_id)))
        .collect();
    let branches: Vec<Value> = state
        .branches
        .iter()
        .rev()
        .map(|b| match parents.get(&b.conversation) {
            Some(parent) => json!({
                "conversation": b.conversation,
                "label": b.label,
                "parent": parent,
            }),
            None => json!({
                "conversation": b.conversation,
                "label": b.label,
            }),
        })
        .collect();

    if let Ok(held) = ctx.get(
        sessions_contract.clone(),
        JsonValue::Object(Default::default()),
    ) {
        let _ = held
            .call(
                "treeSelf",
                JsonValue::from_serde(json!({
                    "main": state.conversation,
                    "branches": branches,
                })),
                &ctx.context,
            )
            .await;
    }
    Ok(())
}

fn last_on(
    id: ConversationId,
    at: u64,
    own: &HashMap<ConversationId, Vec<TreeNode>>,
    parents: &HashMap<ConversationId, Option<pi_rs_durable::ConversationParent>>,
) -> Option<u64> {
    if let Some(found) = own
        .get(&id)
        .and_then(|nodes| nodes.iter().rfind(|n| n.entry <= at))
    {
        return Some(found.entry);
    }
    let parent = parents.get(&id).and_then(|p| p.as_ref())?;
    last_on(parent.conversation_id, at.min(parent.at), own, parents)
}

pub async fn read_tree(session: &Session, context: &Context) -> Result<Tree> {
    let records = top_level_records(session, context)
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let parents: HashMap<ConversationId, Option<pi_rs_durable::ConversationParent>> =
        records.iter().map(|r| (r.id, r.parent)).collect();
    let mut own: HashMap<ConversationId, Vec<TreeNode>> = HashMap::new();

    for record in &records {
        let mut nodes = Vec::new();
        let mut previous = record
            .parent
            .as_ref()
            .and_then(|p| last_on(p.conversation_id, p.at, &own, &parents));
        let min_entry_id = Some(record.parent.as_ref().map_or(1, |p| p.at + 1));
        let mut cursor = None;
        loop {
            let page = session
                .scan_entries(
                    &EntryQuery {
                        conversation_id: record.id,
                        min_entry_id,
                        max_entry_id: None,
                        order: Some(ScanOrder::Ascending),
                    },
                    PAGE,
                    cursor.as_ref(),
                    context,
                )
                .await
                .map_err(|e| Error::Invalid(e.to_string()))?;
            for entry in &page.items {
                if let Some((role, text)) = preview_of(entry) {
                    nodes.push(TreeNode {
                        entry: entry.id,
                        parent: previous,
                        role,
                        text,
                        conversation: record.id,
                    });
                    previous = Some(entry.id);
                }
            }
            if page.next.is_none() {
                break;
            }
            cursor = page.next;
        }
        own.insert(record.id, nodes);
    }

    let state = main_state(session, context)
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let mut nodes: Vec<TreeNode> = own.values().flatten().cloned().collect();
    nodes.sort_by_key(|n| n.entry);
    let current = last_on(state.conversation, u64::MAX, &own, &parents);
    let branches = state
        .branches
        .into_iter()
        .map(|b| TreeBranch {
            conversation: b.conversation,
            label: b.label,
        })
        .collect();

    Ok(Tree {
        nodes,
        branches,
        main: state.conversation,
        current,
    })
}

async fn branch_ending_at(
    session: &Session,
    main: ConversationId,
    point: Option<EntryId>,
    context: &Context,
) -> Result<Option<ConversationId>> {
    let branches = top_level_conversations(session, context)
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let mut ordered = vec![main];
    ordered.extend(branches.into_iter().filter(|&id| id != main));
    for id in ordered {
        let page = session
            .scan_entries(
                &EntryQuery {
                    conversation_id: id,
                    min_entry_id: None,
                    max_entry_id: None,
                    order: Some(ScanOrder::Descending),
                },
                1,
                None,
                context,
            )
            .await
            .map_err(|e| Error::Invalid(e.to_string()))?;
        let tail = page.items.first().map(|e| e.id);
        if tail == point {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

pub async fn navigate(
    session: &Session,
    entry_id: EntryId,
    context: &Context,
) -> Result<(ConversationId, Option<String>)> {
    let main = main_conversation(session, context)
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let chosen = session
        .commit(async move |tx| tx.entry(entry_id).await, context)
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?
        .ok_or_else(|| ChordError::invalid("args", "Unknown message", None))?;
    if !is_top_level(session, chosen.conversation_id, context)
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?
    {
        return Err(ChordError::invalid("args", "Unknown message", None).into());
    }
    let role = role_of(&chosen)
        .ok_or_else(|| ChordError::invalid("args", "Only messages can be chosen", None))?;

    let point = if role == TreeRole::User {
        if entry_id <= 1 {
            None
        } else {
            let page = session
                .scan_entries(
                    &EntryQuery {
                        conversation_id: chosen.conversation_id,
                        min_entry_id: None,
                        max_entry_id: Some(entry_id - 1),
                        order: Some(ScanOrder::Descending),
                    },
                    1,
                    None,
                    context,
                )
                .await
                .map_err(|e| Error::Invalid(e.to_string()))?;
            page.items.into_iter().next()
        }
    } else {
        Some(chosen.clone())
    };
    let text = (role == TreeRole::User).then(|| user_text(&chosen));
    let target = branch_ending_at(session, main, point.as_ref().map(|p| p.id), context).await?;
    if target == Some(main) {
        return Ok((main, text));
    }

    let agent_snapshot = if target.is_none() && point.is_none() {
        let addr = resolve_address(&AGENT_DOC, Some(chosen.conversation_id), None)
            .map_err(|e| Error::Invalid(e.to_string()))?;
        session
            .snapshot_as_of(&AGENT_DOC, &addr, entry_id, context)
            .await
            .map_err(|e| Error::Invalid(e.to_string()))?
    } else {
        None
    };

    let label = preview_of(&chosen).map(|(_, t)| t);
    let main_doc_tok = main_doc();
    let main_addr =
        resolve_address(&main_doc_tok, None, None).map_err(|e| Error::Invalid(e.to_string()))?;

    let conversation = session
        .commit(
            async move |tx| {
                let to = match target {
                    Some(existing) => existing,
                    None => {
                        if let Some(pt) = &point {
                            tx.fork_conversation(
                                pt.conversation_id,
                                pt.id,
                                ConversationOwnership::Ownerless,
                            )
                            .await?
                            .id
                        } else {
                            let created = tx
                                .create_conversation(ConversationOwnership::Ownerless)
                                .await?
                                .id;
                            if let Some(agent) = &agent_snapshot {
                                let new_addr = resolve_address(&AGENT_DOC, Some(created), None)
                                    .map_err(|e| DurableError::invalid(e.to_string()))?;
                                let doc = tx.doc(&AGENT_DOC, &new_addr, None).await?;
                                for (k, v) in agent.iter() {
                                    doc.set(&[Segment::Key(k.clone().into())], v.clone().into())
                                        .map_err(|e| DurableError::invalid(e.to_string()))?;
                                }
                            }
                            created
                        }
                    }
                };
                let doc = tx.doc(&main_doc_tok, &main_addr, None).await?;
                let current_val = doc
                    .value()
                    .map_err(|e| DurableError::invalid(e.to_string()))?
                    .to_json()
                    .to_serde()
                    .map_err(|e| DurableError::invalid(e.to_string()))?;
                let mut state: MainState = serde_json::from_value(current_val)
                    .map_err(|e| DurableError::invalid(e.to_string()))?;
                state.conversation = to;
                if !state.branches.iter().any(|b| b.conversation == to) {
                    state.branches.push(MainBranch {
                        conversation: to,
                        label: label.clone(),
                    });
                }
                doc.set(
                    &[Segment::Key("conversation".into())],
                    json!(state.conversation).into(),
                )
                .map_err(|e| DurableError::invalid(e.to_string()))?;
                doc.set(
                    &[Segment::Key("branches".into())],
                    serde_json::to_value(&state.branches)
                        .map_err(|e| DurableError::invalid(e.to_string()))?
                        .into(),
                )
                .map_err(|e| DurableError::invalid(e.to_string()))?;
                Ok(to)
            },
            context,
        )
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;

    Ok((conversation, text))
}

pub async fn branch(
    session: &Session,
    id: ConversationId,
    label: &str,
    context: &Context,
) -> Result<ConversationId> {
    let text = one_line(label);
    if text.is_empty() {
        return Err(ChordError::invalid(
            "args",
            "A branch label needs at least one character",
            None,
        )
        .into());
    }
    if !is_top_level(session, id, context)
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?
    {
        return Err(ChordError::invalid("args", "Unknown branch", None).into());
    }
    let page = session
        .scan_entries(
            &EntryQuery {
                conversation_id: id,
                min_entry_id: None,
                max_entry_id: None,
                order: Some(ScanOrder::Descending),
            },
            1,
            None,
            context,
        )
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let tail = page.items.into_iter().next().ok_or_else(|| {
        ChordError::invalid(
            "args",
            "Nothing to branch from yet: the conversation has no messages",
            None,
        )
    })?;

    let main_doc_tok = main_doc();
    let main_addr =
        resolve_address(&main_doc_tok, None, None).map_err(|e| Error::Invalid(e.to_string()))?;
    let fork = session
        .commit(
            async move |tx| {
                let fork = tx
                    .fork_conversation(
                        tail.conversation_id,
                        tail.id,
                        ConversationOwnership::Ownerless,
                    )
                    .await?
                    .id;
                let doc = tx.doc(&main_doc_tok, &main_addr, None).await?;
                let current_val = doc
                    .value()
                    .map_err(|e| DurableError::invalid(e.to_string()))?
                    .to_json()
                    .to_serde()
                    .map_err(|e| DurableError::invalid(e.to_string()))?;
                let mut state: MainState = serde_json::from_value(current_val)
                    .map_err(|e| DurableError::invalid(e.to_string()))?;
                state.conversation = fork;
                state.branches.push(MainBranch {
                    conversation: fork,
                    label: Some(text.clone()),
                });
                doc.set(
                    &[Segment::Key("conversation".into())],
                    json!(state.conversation).into(),
                )
                .map_err(|e| DurableError::invalid(e.to_string()))?;
                doc.set(
                    &[Segment::Key("branches".into())],
                    serde_json::to_value(&state.branches)
                        .map_err(|e| DurableError::invalid(e.to_string()))?
                        .into(),
                )
                .map_err(|e| DurableError::invalid(e.to_string()))?;
                Ok(fork)
            },
            context,
        )
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;

    Ok(fork)
}

pub async fn select(session: &Session, id: ConversationId, context: &Context) -> Result<()> {
    if !is_top_level(session, id, context)
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?
    {
        return Err(ChordError::invalid("args", "Unknown branch", None).into());
    }
    let mut state = main_state(session, context)
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;
    if state.conversation == id {
        return Ok(());
    }
    state.conversation = id;
    set_main_state(session, &state, context)
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;
    Ok(())
}

pub async fn rename(
    session: &Session,
    id: ConversationId,
    label: Option<&str>,
    context: &Context,
) -> Result<()> {
    let text = label
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if text.chars().map(char::len_utf16).sum::<usize>() > PREVIEW_LENGTH {
        return Err(ChordError::invalid(
            "args",
            format!("A branch label has at most {PREVIEW_LENGTH} characters"),
            None,
        )
        .into());
    }
    if text.is_empty() && id != ROOT_CONVERSATION_ID {
        return Err(ChordError::invalid(
            "args",
            "A branch label needs at least one character",
            None,
        )
        .into());
    }
    let mut state = main_state(session, context)
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let Some(branch) = state.branches.iter_mut().find(|b| b.conversation == id) else {
        return Err(ChordError::invalid("args", "Unknown branch", None).into());
    };
    branch.label = (!text.is_empty()).then_some(text);
    set_main_state(session, &state, context)
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;
    Ok(())
}

pub fn worker_facet() -> Result<Facet> {
    let tree_contract = session_tree_contract()?;
    let sessions_contract = Arc::new(lookup("p2.sessions")?);

    Ok(define_facet(move |host| {
        let host_handle = host.host();
        let worker = worker_of(&host_handle)?;

        let host_ro = host_handle.clone();
        worker
            .registries
            .read_only
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                "tree".into(),
                Arc::new(move |id, ctx| {
                    let host_ro = host_ro.clone();
                    Box::pin(async move {
                        let worker = worker_of(&host_ro)?;
                        let session = &worker.harness()?.durable.session;
                        let main = main_conversation(session, &ctx)
                            .await
                            .map_err(|e| Error::Invalid(e.to_string()))?;
                        if id != main
                            && is_top_level(session, id, &ctx)
                                .await
                                .map_err(|e| Error::Invalid(e.to_string()))?
                        {
                            Ok(Some(NOT_CURRENT.to_string()))
                        } else {
                            Ok(None)
                        }
                    })
                }),
            );

        let sessions_c = sessions_contract.clone();
        host.serve(
            tree_contract.clone(),
            move |_params, principal, _object| {
                if !is_owner(principal) {
                    return Err(ChordError::new("forbidden", "the tree is the owner's").into());
                }
                let mut members = Members::variant("owner");

                let host_t = host_handle.clone();
                members.method("tree", move |_, ctx| {
                    let host_t = host_t.clone();
                    Box::pin(async move {
                        let worker = worker_of(&host_t)?;
                        let parts = worker.harness()?;
                        let session = &parts.durable.session;
                        let tree = parts
                            .boundary
                            .liveness()
                            .call(read_tree(session, &ctx.context))
                            .await?;
                        Ok(JsonValue::from_serde(serde_json::to_value(tree)?))
                    })
                })?;

                let host_n = host_handle.clone();
                let sess_n = sessions_c.clone();
                members.method("navigate", move |args, ctx| {
                    let host_n = host_n.clone();
                    let sess_n = sess_n.clone();
                    Box::pin(async move {
                        let entry = args
                            .to_serde()?
                            .get("entry")
                            .and_then(Value::as_u64)
                            .filter(|&e| e >= 1)
                            .ok_or_else(|| ChordError::invalid("args", "missing entry", None))?;
                        let worker = worker_of(&host_n)?;
                        let parts = worker.harness()?;
                        let session = &parts.durable.session;
                        let (conversation, text) = parts
                            .boundary
                            .liveness()
                            .call(async {
                                let res = navigate(session, entry, &ctx.context).await?;
                                report_tree(session, &sess_n, &ctx).await?;
                                Ok(res)
                            })
                            .await?;
                        Ok(JsonValue::from_serde(json!({
                            "conversation": conversation,
                            "text": text,
                        })))
                    })
                })?;

                let host_s = host_handle.clone();
                let sess_s = sessions_c.clone();
                members.method("select", move |args, ctx| {
                    let host_s = host_s.clone();
                    let sess_s = sess_s.clone();
                    Box::pin(async move {
                        let conv = args
                            .to_serde()?
                            .get("conversation")
                            .and_then(Value::as_u64)
                            .filter(|&c| c >= 1)
                            .ok_or_else(|| {
                                ChordError::invalid("args", "missing conversation", None)
                            })?;
                        let worker = worker_of(&host_s)?;
                        let parts = worker.harness()?;
                        let session = &parts.durable.session;
                        parts
                            .boundary
                            .liveness()
                            .call(async {
                                select(session, conv, &ctx.context).await?;
                                report_tree(session, &sess_s, &ctx).await?;
                                Ok(())
                            })
                            .await?;
                        Ok(JsonValue::Null)
                    })
                })?;

                let host_b = host_handle.clone();
                let sess_b = sessions_c.clone();
                members.method("branch", move |args, ctx| {
                    let host_b = host_b.clone();
                    let sess_b = sess_b.clone();
                    Box::pin(async move {
                        let v = args.to_serde()?;
                        let conv = v
                            .get("conversation")
                            .and_then(Value::as_u64)
                            .filter(|&c| c >= 1)
                            .ok_or_else(|| {
                                ChordError::invalid("args", "missing conversation", None)
                            })?;
                        let label = v
                            .get("label")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .ok_or_else(|| ChordError::invalid("args", "missing label", None))?;
                        let worker = worker_of(&host_b)?;
                        let parts = worker.harness()?;
                        let session = &parts.durable.session;
                        let fork = parts
                            .boundary
                            .liveness()
                            .call(async {
                                let fork = branch(session, conv, label, &ctx.context).await?;
                                report_tree(session, &sess_b, &ctx).await?;
                                Ok(fork)
                            })
                            .await?;
                        Ok(JsonValue::from_serde(json!({ "conversation": fork })))
                    })
                })?;

                let host_r = host_handle.clone();
                let sess_r = sessions_c.clone();
                members.method("rename", move |args, ctx| {
                    let host_r = host_r.clone();
                    let sess_r = sess_r.clone();
                    Box::pin(async move {
                        let v = args.to_serde()?;
                        let conv = v
                            .get("conversation")
                            .and_then(Value::as_u64)
                            .filter(|&c| c >= 1)
                            .ok_or_else(|| {
                                ChordError::invalid("args", "missing conversation", None)
                            })?;
                        let label = v.get("label").and_then(Value::as_str);
                        let worker = worker_of(&host_r)?;
                        let parts = worker.harness()?;
                        let session = &parts.durable.session;
                        parts
                            .boundary
                            .liveness()
                            .call(async {
                                rename(session, conv, label, &ctx.context).await?;
                                report_tree(session, &sess_r, &ctx).await?;
                                Ok(())
                            })
                            .await?;
                        Ok(JsonValue::Null)
                    })
                })?;

                Ok(members)
            },
            ServeOptions::default(),
        )?;

        Ok(())
    }))
}

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    registry.register("tree", FacetKind::Worker, worker_facet()?)
}
