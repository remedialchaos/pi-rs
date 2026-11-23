use indexmap::IndexMap;
use pi_rs_chord::{
    Result,
    contracts::{HostKind, read_params, schema},
    store::{ColumnRecord, ColumnType, Query, StoreEngine, TableDefinition, WhereNode},
};
use serde_json::{Value, json};

use crate::{api::v1::GrantMode, runtime::address::session_address};

const OWNER: &str = "p2.shares";

pub(super) struct Tables {
    pub(super) links: TableDefinition,
    pub(super) guests: TableDefinition,
    pub(super) grants: TableDefinition,
}

impl Tables {
    pub(super) fn new() -> Result<Self> {
        let host = HostKind::new(
            "session",
            read_params(&[("id".into(), schema::str())], "session")?,
        )?;
        let text = || ColumnRecord::new(ColumnType::Str);
        let integer = || ColumnRecord::new(ColumnType::Int);
        let column = || ColumnRecord::host("session");

        let links = TableDefinition::new(
            "links",
            1,
            IndexMap::from([
                ("session".into(), column()),
                ("mode".into(), text()),
                ("id".into(), text()),
                ("token".into(), text()),
            ]),
            vec!["session".into(), "mode".into()],
            IndexMap::from([
                ("by_token".into(), vec!["token".into()]),
                ("by_id".into(), vec!["id".into()]),
            ]),
        )?
        .bind_host("session", host.clone())?;

        let guests = TableDefinition::new(
            "guests",
            1,
            IndexMap::from([
                ("id".into(), text()),
                ("name".into(), text()),
                ("secretHash".into(), text().optional()),
                ("email".into(), text().optional()),
                ("revision".into(), integer()),
                ("lastSeen".into(), integer()),
            ]),
            vec!["id".into()],
            IndexMap::from([("by_name".into(), vec!["name".into()])]),
        )?;

        let grants = TableDefinition::new(
            "grants",
            1,
            IndexMap::from([
                ("session".into(), column()),
                ("guest".into(), text()),
                ("mode".into(), text()),
                ("linkId".into(), text().optional()),
                ("removed".into(), ColumnRecord::new(ColumnType::Bool)),
                ("grantedAt".into(), integer()),
            ]),
            vec!["session".into(), "guest".into()],
            IndexMap::from([("by_guest".into(), vec!["guest".into()])]),
        )?
        .bind_host("session", host)?;

        Ok(Self {
            links,
            guests,
            grants,
        })
    }

    pub(super) async fn current_grant(
        &self,
        store: &StoreEngine,
        session: &str,
        guest: &str,
        revision: u64,
        mode: GrantMode,
    ) -> Result<bool> {
        let address = session_address(session);
        let query = Query::new(OWNER, self.grants.clone())?
            .filter(WhereNode::Eq(("session".into(), address["key"].clone())))?
            .filter(WhereNode::Eq(("guest".into(), json!(guest))))?
            .with_limit(1)?;
        let page = store.read(query).await?;
        let Some(grant) = page.rows.first() else {
            return Ok(false);
        };
        if grant["removed"] != false || grant["mode"] != serde_json::to_value(mode)? {
            return Ok(false);
        }
        if let Some(link) = grant.get("linkId").and_then(Value::as_str) {
            let query = Query::new(OWNER, self.links.clone())?
                .filter(WhereNode::Eq(("id".into(), json!(link))))?
                .filter(WhereNode::Eq(("session".into(), address["key"].clone())))?
                .filter(WhereNode::Eq(("mode".into(), grant["mode"].clone())))?;
            if store.count(query).await? == 0 {
                return Ok(false);
            }
        }

        // Grant changes bump the guest revision in the same commit. Read the
        // revision last so a concurrent withdrawal invalidates a prior row read.
        let query = Query::new(OWNER, self.guests.clone())?
            .filter(WhereNode::Eq(("id".into(), json!(guest))))?
            .with_limit(1)?;
        let page = store.read(query).await?;
        Ok(page.rows.first().and_then(|row| row["revision"].as_u64()) == Some(revision))
    }
}

#[cfg(test)]
mod tests;
