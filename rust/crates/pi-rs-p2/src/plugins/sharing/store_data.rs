use super::store::Tables;
use crate::{api::v1::GrantMode, runtime::address::session_address};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use indexmap::IndexMap;
use pi_rs_chord::{
    Error, Result,
    data::ChordError,
    sqlite::SqliteValue,
    store::{Step, StoreEngine, StoreWrite},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::{Arc, LazyLock};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

const OWNER: &str = "p2.shares";
static GUEST_NAME_FILTER: LazyLock<std::result::Result<regex::Regex, regex::Error>> =
    LazyLock::new(|| regex::Regex::new(r"[\p{C}<>]"));

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Link {
    pub id: String,
    pub session: String,
    pub mode: GrantMode,
    pub token: String,
}

#[derive(Serialize)]
pub(super) struct Joined {
    pub session: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cookie: Option<String>,
}

fn text(value: &str) -> SqliteValue {
    SqliteValue::Text(value.into())
}

fn string(cell: &SqliteValue) -> Result<&str> {
    match cell {
        SqliteValue::Text(value) => Ok(value),
        _ => Err(Error::Invalid("Invalid sharing table cell".into())),
    }
}

fn int(cell: &SqliteValue) -> Result<i64> {
    match cell {
        SqliteValue::Integer(value) => Ok(*value),
        _ => Err(Error::Invalid("Invalid sharing table integer".into())),
    }
}

pub(super) fn time_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or_default()
}

fn secret() -> [u8; 32] {
    let mut bytes = [0; 32];
    bytes[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    bytes
}

fn hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

fn revise(step: &mut Step<'_>, id: &str) -> Result<()> {
    step.apply(StoreWrite::update_where(
        "p2.shares:guests",
        pi_rs_chord::store::WhereNode::Eq(("id".into(), json!(id))),
        IndexMap::from([(
            "revision".into(),
            pi_rs_chord::store::ExprNode(json!({
                "add":[{"col":"revision"},1]
            })),
        )]),
        IndexMap::new(),
    ))
}

fn session_key(session: &str) -> String {
    pi_rs_chord::data::join_key(&["session".into(), session.into()])
}

pub(super) fn join_error(message: &str) -> Error {
    ChordError::new("invalid", message).into()
}

pub(super) async fn link(
    store: &StoreEngine,
    tables: Arc<Tables>,
    session: String,
    mode: GrantMode,
    reset: bool,
) -> Result<Vec<String>> {
    store
        .transact(move |step| {
            let address = session_address(&session);
            let key = session_key(&session);
            let mode = serde_json::to_value(mode)?;
            let mode_name = mode
                .as_str()
                .ok_or_else(|| Error::Invalid("invalid mode".into()))?;
            let existing = step.read(
                r#"SELECT id FROM "p2.shares:links" WHERE session=? AND mode=?"#,
                vec![text(&key), text(mode_name)],
            )?;
            let mut ended = Vec::new();
            if let Some(row) = existing.first() {
                if !reset {
                    return Ok(ended);
                }
                let old = string(&row[0])?;
                for row in step.read(
                    r#"SELECT guest FROM "p2.shares:grants" WHERE session=? AND linkId=?"#,
                    vec![text(&key), text(old)],
                )? {
                    let id = string(&row[0])?;
                    step.apply(StoreWrite::Delete {
                        table: "p2.shares:grants".into(),
                        key: IndexMap::from([
                            ("session".into(), json!(key)),
                            ("guest".into(), json!(id)),
                        ]),
                        count: None,
                    })?;
                    revise(step, id)?;
                    ended.push(id.to_owned());
                }
            }
            step.apply(tables.links.put(
                OWNER,
                json!({
                    "session":address, "mode":mode, "id":Uuid::new_v4().to_string(),
                    "token":URL_SAFE_NO_PAD.encode(secret()),
                }),
            )?)?;
            Ok(ended)
        })
        .await
}

pub(super) async fn remove(
    store: &StoreEngine,
    session: String,
    guest: String,
) -> Result<Vec<String>> {
    store.transact(move |step| {
        let key = session_key(&session);
        let condition = pi_rs_chord::store::WhereNode::And(vec![
            pi_rs_chord::store::WhereNode::Eq(("session".into(), json!(key))),
            pi_rs_chord::store::WhereNode::Eq(("guest".into(), json!(guest))),
            pi_rs_chord::store::WhereNode::Eq(("removed".into(), json!(false))),
        ]);
        if step.read(r#"SELECT 1 FROM "p2.shares:grants" WHERE session=? AND guest=? AND NOT removed"#, vec![text(&key), text(&guest)])?.is_empty() {
            return Ok(Vec::new());
        }
        step.apply(StoreWrite::update_where(
            "p2.shares:grants", condition,
            IndexMap::from([("removed".into(), pi_rs_chord::store::ExprNode(json!(true)))]),
            IndexMap::new(),
        ))?;
        revise(step, &guest)?;
        Ok(vec![guest])
    }).await
}

pub(super) async fn share(
    store: &StoreEngine,
    tables: Arc<Tables>,
    session: String,
    name: String,
    email: String,
    mode: GrantMode,
) -> Result<Vec<String>> {
    store
        .transact(move |step| {
            let found = step.read(
                r#"SELECT id FROM "p2.shares:guests" WHERE email=? LIMIT 1"#,
                vec![text(&email)],
            )?;
            let mut changed = Vec::new();
            let id = if let Some(row) = found.first() {
                let id = string(&row[0])?.to_owned();
                revise(step, &id)?;
                changed.push(id.clone());
                id
            } else {
                let id = Uuid::new_v4().to_string();
                step.apply(tables.guests.put(
                    OWNER,
                    json!({
                        "id":id, "name":name, "email":email, "revision":1, "lastSeen":0,
                    }),
                )?)?;
                id
            };
            step.apply(tables.grants.put(
                OWNER,
                json!({
                    "session":session_address(&session), "guest":id, "mode":mode,
                    "removed":false, "grantedAt":time_ms(),
                }),
            )?)?;
            Ok(changed)
        })
        .await
}

pub(super) async fn join(
    store: &StoreEngine,
    tables: Arc<Tables>,
    link: Link,
    name: String,
    cookie: Option<String>,
) -> Result<(Joined, Vec<String>)> {
    store.transact(move |step| {
        let key = session_key(&link.session);
        if step.read(r#"SELECT 1 FROM "p2.shares:links" WHERE session=? AND id=? AND token=?"#,
            vec![text(&key), text(&link.id), text(&link.token)])?.is_empty() {
            return Err(join_error("This share has ended"));
        }
        let existing = if let Some((id, digest)) = cookie.as_deref().and_then(cookie_hash) {
            let rows = step.read(r#"SELECT 1 FROM "p2.shares:guests" WHERE id=? AND secretHash=?"#, vec![text(id), text(&digest)])?;
            (!rows.is_empty()).then(|| id.to_owned())
        } else {
            None
        };
        if let Some(id) = &existing {
            let rows = step.read(r#"SELECT removed, linkId FROM "p2.shares:grants" WHERE session=? AND guest=?"#,
                vec![text(&key), text(id)])?;
            if let Some(row) = rows.first() {
                if int(&row[0])? != 0 {
                    return Err(join_error("You were removed from this session"));
                }
                if row[1] == text(&link.id) {
                    return Ok((Joined { session: link.session, cookie: None }, Vec::new()));
                }
            }
        }
        let count = step.read(r#"SELECT COUNT(*) FROM "p2.shares:grants" WHERE session=? AND linkId=?"#,
            vec![text(&key), text(&link.id)])?;
        if count.first().and_then(|row| row.first()).map(int).transpose()?.unwrap_or_default() >= 1000 {
            return Err(join_error("Guest limit reached"));
        }
        let mut changed = Vec::new();
        let (id, cookie) = if let Some(id) = existing {
            revise(step, &id)?;
            changed.push(id.clone());
            (id, None)
        } else {
            let name = unique_name(step, &name)?;
            let id = Uuid::new_v4().to_string();
            let key = secret().iter().map(|byte| format!("{byte:02x}")).collect::<String>();
            step.apply(tables.guests.put(OWNER, json!({
                "id":id, "name":name, "secretHash":hash(&key), "revision":1, "lastSeen":time_ms(),
            }))?)?;
            let cookie = format!("{id}.{key}");
            (id, Some(cookie))
        };
        step.apply(tables.grants.put(OWNER, json!({
            "session":session_address(&link.session), "guest":id, "mode":link.mode, "linkId":link.id,
            "removed":false, "grantedAt":time_ms(),
        }))?)?;
        Ok((Joined { session: link.session, cookie }, changed))
    }).await
}

fn unique_name(step: &Step<'_>, requested: &str) -> Result<String> {
    let normalized: String = requested.nfkc().collect();
    let filter = GUEST_NAME_FILTER
        .as_ref()
        .map_err(|error| Error::Invalid(error.to_string()))?;
    let clean = filter.replace_all(&normalized, "");
    let name = String::from_utf16_lossy(&clean.trim().encode_utf16().take(40).collect::<Vec<_>>());
    if name.is_empty() {
        return Err(join_error("Choose a guest name"));
    }
    let parent = format!("{name} (");
    let mut count = 0;
    for row in step.read(r#"SELECT name FROM "p2.shares:guests""#, vec![])? {
        let previous = string(&row[0])?;
        if previous == name || (previous.starts_with(&parent) && previous.ends_with(')')) {
            count += 1;
        }
    }
    Ok(if count == 0 {
        name
    } else {
        format!("{name} ({})", count + 1)
    })
}

pub(super) fn cookie_hash(cookie: &str) -> Option<(&str, String)> {
    let (id, secret) = cookie.split_once('.')?;
    (!id.is_empty() && secret.len() == 64).then(|| (id, hash(secret)))
}

pub(super) async fn seen(store: &StoreEngine, id: String) -> Result<()> {
    store
        .commit(vec![StoreWrite::update_where(
            "p2.shares:guests",
            pi_rs_chord::store::WhereNode::Eq(("id".into(), json!(id))),
            IndexMap::from([(
                "lastSeen".into(),
                pi_rs_chord::store::ExprNode(json!(time_ms())),
            )]),
            IndexMap::new(),
        )])
        .await?;
    Ok(())
}

pub(super) async fn bump(store: &StoreEngine, session: String) -> Result<Vec<String>> {
    store
        .transact(move |step| {
            let rows = step.read(
                r#"SELECT guest FROM "p2.shares:grants" WHERE session=?"#,
                vec![text(&session_key(&session))],
            )?;
            let mut ids = Vec::new();
            for row in rows {
                let id = string(&row[0])?;
                revise(step, id)?;
                ids.push(id.to_owned());
            }
            Ok(ids)
        })
        .await
}
