use super::{
    http,
    store::Tables,
    store_data::{self, Link},
};
use crate::{
    api::v1::{GrantMode, Principal, p2_contracts::lookup},
    runtime::server::server_of,
};
use indexmap::IndexMap;
use pi_rs_chord::{
    Error, Result,
    context::Context,
    contracts::Contract,
    data::ChordError,
    delta::Revision,
    host::{Facet, Held, Host, Members, ObjectSelf, ServeOptions, define_facet},
    json::JsonValue,
    state::Source,
    store::{Query, StoreEngine, TableDefinition, WhereNode},
    watch::NativeWatchCall,
};
use serde_json::{Value, json};
use std::{collections::BTreeSet, sync::Arc};

pub(super) const OWNER: &str = "p2.shares";

fn wire(value: Value) -> JsonValue {
    JsonValue::from_serde(value)
}

fn arg<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args[key]
        .as_str()
        .ok_or_else(|| Error::Invalid(format!("Missing {key}")))
}

async fn rows(
    store: &StoreEngine,
    table: &TableDefinition,
    filter: Option<WhereNode>,
) -> Result<Vec<Value>> {
    let mut query = Query::new(OWNER, table.clone())?.with_limit(1000)?;
    if let Some(filter) = filter {
        query = query.filter(filter)?;
    }
    let mut results = Vec::new();
    loop {
        let page = store.read(query.clone()).await?;
        results.extend(page.rows);
        if page.cursor.is_none() {
            return Ok(results);
        }
        query = query.after(page.cursor.as_deref())?;
    }
}

fn equals(field: &str, value: impl Into<Value>) -> Option<WhereNode> {
    Some(WhereNode::Eq((field.into(), value.into())))
}

pub(super) async fn call(
    host: &Host,
    contract: Arc<Contract>,
    method: &str,
    args: Value,
    ctx: &Context,
) -> Result<Value> {
    host.get(contract, wire(json!({})), ctx)
        .await?
        .call(method, wire(args), ctx)
        .await?
        .to_serde()
}

pub(super) async fn catalog_row(
    host: &Host,
    session: &str,
    ctx: &Context,
) -> Result<Option<Value>> {
    let contract = Arc::new(lookup("p2.catalog")?);
    let result = call(host, contract, "get", json!({"id":session}), ctx).await;
    Ok(result
        .ok()
        .filter(|row| row["id"] == session && row["archivedAt"].is_null()))
}

fn link_row(row: Value) -> Result<Link> {
    Ok(Link {
        id: arg(&row, "id")?.into(),
        session: arg(&row["session"]["params"], "id")?.into(),
        mode: serde_json::from_value(row["mode"].clone())?,
        token: arg(&row, "token")?.into(),
    })
}

#[derive(Clone)]
struct Shares {
    host: pi_rs_chord::host::WeakHost,
    store: StoreEngine,
    tables: Arc<Tables>,
    publishing: Arc<tokio::sync::Mutex<()>>,
}

impl Shares {
    fn host(&self) -> Result<Host> {
        self.host.upgrade().ok_or(Error::Closed)
    }

    async fn active(&self, token: &str, ctx: &Context) -> Result<Option<Link>> {
        if token.len() != 43
            || !token
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
        {
            return Ok(None);
        }
        let found = rows(&self.store, &self.tables.links, equals("token", token))
            .await?
            .into_iter()
            .next()
            .map(link_row)
            .transpose()?;
        let Some(link) = found else {
            return Ok(None);
        };
        Ok(catalog_row(&self.host()?, &link.session, ctx)
            .await?
            .map(|_| link))
    }

    async fn principal(&self, guest: Value, ctx: &Context) -> Result<Value> {
        let id = arg(&guest, "id")?;
        let revision = guest["revision"]
            .as_u64()
            .ok_or_else(|| Error::Invalid("Invalid guest revision".into()))?;
        let host = self.host()?;
        let grants = rows(&self.store, &self.tables.grants, equals("guest", id)).await?;
        let mut sessions = IndexMap::new();
        let mut home = None;
        for grant in grants {
            let session = arg(&grant["session"]["params"], "id")?;
            let mode = serde_json::from_value(grant["mode"].clone())?;
            if grant["removed"] != false
                || catalog_row(&host, session, ctx).await?.is_none()
                || !self
                    .tables
                    .current_grant(&self.store, session, id, revision, mode)
                    .await?
            {
                continue;
            }
            sessions.insert(session.to_owned(), mode);
            let at = grant["grantedAt"].as_i64().unwrap_or_default();
            if home.as_ref().is_none_or(|(_, previous)| *previous <= at) {
                home = Some((session.to_owned(), at));
            }
        }
        let Some((home, _)) = home else {
            return Ok(Value::Null);
        };
        Ok(serde_json::to_value(Principal::Guest {
            id: id.into(),
            name: arg(&guest, "name")?.into(),
            email: guest["email"].as_str().map(str::to_owned),
            home,
            sessions,
            revision,
        })?)
    }

    async fn revoke(&self, ids: Vec<String>, ctx: &Context) -> Result<()> {
        let host = self.host()?;
        for id in ids {
            call(
                &host,
                Arc::new(lookup("p2.server")?),
                "revokeGuests",
                json!({"id":id}),
                ctx,
            )
            .await?;
        }
        Ok(())
    }

    async fn confirm(&self, args: &Value, ctx: &Context) -> Result<(String, GrantMode)> {
        let session = arg(args, "session")?;
        let host = self.host()?;
        let row = catalog_row(&host, session, ctx)
            .await?
            .ok_or_else(|| store_data::join_error("Session not found or archived"))?;
        let mode = serde_json::from_value(args["mode"].clone())?;
        if mode == GrantMode::Full
            && args["remoteConfirmed"] != true
            && !environments(&host, session, &row, ctx).await?.is_empty()
        {
            return Err(store_data::join_error(
                "Confirm first: guests with send access can run commands on this remote host",
            ));
        }
        Ok((session.into(), mode))
    }

    async fn execute(&self, method: &str, args: Value, ctx: &Context) -> Result<Value> {
        match method {
            "active" => Ok(serde_json::to_value(
                self.active(arg(&args, "token")?, ctx).await?,
            )?),
            "grant" | "authenticate" | "authenticateEmail" => {
                let mut cookie_secret = None;
                let condition = match method {
                    "authenticate" => {
                        let Some((id, digest)) = store_data::cookie_hash(arg(&args, "cookie")?)
                        else {
                            return Ok(Value::Null);
                        };
                        cookie_secret = Some(digest);
                        equals("id", id)
                    }
                    "authenticateEmail" => equals("email", arg(&args, "email")?),
                    _ => equals("id", arg(&args, "id")?),
                };
                let Some(guest) = rows(&self.store, &self.tables.guests, condition)
                    .await?
                    .into_iter()
                    .next()
                else {
                    return Ok(Value::Null);
                };
                if cookie_secret
                    .as_ref()
                    .is_some_and(|digest| guest["secretHash"].as_str() != Some(digest))
                {
                    return Ok(Value::Null);
                }
                let grant = self.principal(guest, ctx).await?;
                if method != "grant"
                    && let Some(id) = grant["id"].as_str()
                {
                    store_data::seen(&self.store, id.into()).await?;
                }
                Ok(grant)
            }
            "link" => {
                let (session, mode) = self.confirm(&args, ctx).await?;
                self.revoke(
                    store_data::link(
                        &self.store,
                        self.tables.clone(),
                        session,
                        mode,
                        args["reset"] == true,
                    )
                    .await?,
                    ctx,
                )
                .await?;
                Ok(Value::Null)
            }
            "share" => {
                let name = arg(&args, "email")?.trim();
                let email = normalize_email(name)
                    .ok_or_else(|| store_data::join_error("Enter an email address"))?;
                let (session, mode) = self.confirm(&args, ctx).await?;
                self.revoke(
                    store_data::share(
                        &self.store,
                        self.tables.clone(),
                        session,
                        name.into(),
                        email,
                        mode,
                    )
                    .await?,
                    ctx,
                )
                .await?;
                Ok(Value::Null)
            }
            "remove" => {
                self.revoke(
                    store_data::remove(
                        &self.store,
                        arg(&args, "session")?.into(),
                        arg(&args, "guestId")?.into(),
                    )
                    .await?,
                    ctx,
                )
                .await?;
                Ok(Value::Null)
            }
            "join" => {
                let link = self
                    .active(arg(&args, "token")?, ctx)
                    .await?
                    .ok_or_else(|| store_data::join_error("This share has ended"))?;
                let (joined, changed) = store_data::join(
                    &self.store,
                    self.tables.clone(),
                    link,
                    arg(&args, "name")?.into(),
                    args["cookie"].as_str().map(str::to_owned),
                )
                .await?;
                self.revoke(changed, ctx).await?;
                Ok(serde_json::to_value(joined)?)
            }
            _ => Err(Error::Invalid("Unknown sharing method".into())),
        }
    }

    async fn publish(&self, catalog: &Source, source: &Source, ctx: &Context) -> Result<()> {
        let _permit = self.publishing.lock().await;
        source.set(wire(self.snapshot(catalog).await?), ctx)
    }

    async fn snapshot(&self, catalog: &Source) -> Result<Value> {
        let catalog: Vec<Value> = serde_json::from_value(catalog.value()?.to_json().to_serde()?)?;
        let open = catalog
            .iter()
            .filter(|row| row["archivedAt"].is_null())
            .filter_map(|row| row["id"].as_str())
            .collect::<BTreeSet<_>>();
        let mut links = rows(&self.store, &self.tables.links, None)
            .await?
            .into_iter()
            .map(link_row)
            .collect::<Result<Vec<_>>>()?;
        links.sort_by(|a, b| {
            a.session
                .cmp(&b.session)
                .then_with(|| (a.mode == GrantMode::Full).cmp(&(b.mode == GrantMode::Full)))
        });
        let guests = rows(&self.store, &self.tables.guests, None)
            .await?
            .into_iter()
            .filter_map(|row| row["id"].as_str().map(|id| (id.to_owned(), row.clone())))
            .collect::<IndexMap<_, _>>();
        let mut people = Vec::new();
        let mut emails = BTreeSet::new();
        for grant in rows(&self.store, &self.tables.grants, None).await? {
            if grant["removed"] != false {
                continue;
            }
            let guest_id = arg(&grant, "guest")?;
            let Some(guest) = guests.get(guest_id) else {
                continue;
            };
            let Some(email) = guest["email"].as_str() else {
                continue;
            };
            let session = arg(&grant["session"]["params"], "id")?;
            if open.contains(session) {
                emails.insert(email.to_owned());
            }
            people.push(
                json!({"id":guest_id,"name":guest["name"],"session":session,"mode":grant["mode"]}),
            );
        }
        people.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        Ok(json!({"links":links,"people":people,"emails":emails}))
    }
}

fn normalize_email(input: &str) -> Option<String> {
    let (local, domain) = input.trim().split_once('@')?;
    let local = local.trim().to_lowercase();
    if local.is_empty() || domain.contains('@') {
        return None;
    }
    let url::Host::Domain(domain) = url::Host::parse(domain).ok()? else {
        return None;
    };
    if !domain.contains('.') {
        return None;
    }
    Some(
        if matches!(domain.as_str(), "gmail.com" | "googlemail.com") {
            format!("{}@gmail.com", local.replace('.', ""))
        } else {
            format!("{local}@{domain}")
        },
    )
}

fn environment_names(row: &Value, children: &Value) -> Vec<String> {
    let child_locations = children
        .as_array()
        .into_iter()
        .flatten()
        .map(|child| &child["location"]["env"]);
    std::iter::once(&row["env"])
        .chain(child_locations)
        .filter_map(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect::<indexmap::IndexSet<_>>()
        .into_iter()
        .collect()
}

async fn environments(
    host: &Host,
    session: &str,
    row: &Value,
    ctx: &Context,
) -> Result<Vec<String>> {
    let children = if host.served_contract("p2.child-store").is_some() {
        host.get(Arc::new(lookup("p2.child-store")?), wire(json!({})), ctx)
            .await?
            .member("children")?
            .value()?
            .to_json()
            .to_serde()?
    } else {
        Value::Null
    };
    Ok(environment_names(row, &children[session]))
}

fn derive(
    object: &ObjectSelf,
    input: Source,
    map: impl Fn(Value) -> Result<Value> + Send + Sync + 'static,
) -> Source {
    Source::derive(
        object.batch(),
        vec![input],
        move |values| {
            Ok(Revision::from_json(wire(map(values[0]
                .to_json()
                .to_serde()?)?))?)
        },
        |_| {},
    )
}

fn shares_members(object: &mut ObjectSelf, tables: Arc<Tables>) -> Result<Members> {
    let store = object
        .store()
        .cloned()
        .ok_or_else(|| Error::Invalid("Server store unavailable".into()))?;
    let api = Shares {
        host: object.host().downgrade(),
        store,
        tables,
        publishing: Default::default(),
    };
    for table in [&api.tables.links, &api.tables.guests, &api.tables.grants] {
        object.table(table.clone())?;
    }
    let catalog = object.get(Arc::new(lookup("p2.catalog")?), wire(json!({})))?;
    let sessions = catalog.member("sessions")?;
    let changes = api.store.changes();
    let open_api = api.clone();
    let open_sessions = sessions.clone();
    let snapshot = object.follow(move |state, ctx| {
        Box::pin(async move {
            open_api
                .publish(&open_sessions, &state, &ctx.context)
                .await?;
            Ok(Box::new(|| {}) as Box<dyn FnOnce() + Send>)
        })
    })?;
    let state = snapshot.clone();
    let observer = api.clone();
    let catalog_state = sessions.clone();
    object.on_live(move |ctx| {
        let observer = observer.clone();
        let state = state.clone();
        let catalog = catalog.clone();
        let catalog_state = catalog_state.clone();
        let mut changes = changes.clone();
        Box::pin(async move {
            catalog.wait(&ctx.context).await?;
            let (changed, mut updates) = tokio::sync::watch::channel(());
            let _watch = catalog_state.subscribe(move |_, _| { changed.send_replace(()); Ok(()) }, |_| {});
            let mut previous = None::<BTreeSet<String>>;
            loop {
                changes.borrow_and_update();
                updates.borrow_and_update();
                let rows: Vec<Value> = serde_json::from_value(catalog_state.value()?.to_json().to_serde()?)?;
                let archived = rows.iter().filter(|row| !row["archivedAt"].is_null())
                    .filter_map(|row| row["id"].as_str().map(str::to_owned)).collect::<BTreeSet<_>>();
                if let Some(before) = &previous {
                    for session in before.symmetric_difference(&archived) {
                        let ids = store_data::bump(&observer.store, session.clone()).await?;
                        observer.revoke(ids, &ctx.context).await?;
                    }
                }
                previous = Some(archived);
                observer.publish(&catalog_state, &state, &ctx.context).await?;
                let wake = async {
                    tokio::select! { value = changes.changed() => value, value = updates.changed() => value }
                };
                if !matches!(ctx.context.await_with(wake).await, Ok(Ok(()))) {
                    return Ok(());
                }
            }
        })
    });
    let mut members = Members::new();
    for name in ["links", "people", "emails"] {
        members.state(
            name,
            derive(object, snapshot.clone(), move |value| {
                Ok(value[name].clone())
            }),
        )?;
    }
    for name in [
        "authenticate",
        "authenticateEmail",
        "grant",
        "active",
        "link",
        "share",
        "remove",
        "join",
    ] {
        let api = api.clone();
        let sessions = sessions.clone();
        let snapshot = snapshot.clone();
        members.method(name, move |args, ctx| {
            let api = api.clone();
            let sessions = sessions.clone();
            let snapshot = snapshot.clone();
            Box::pin(async move {
                let value = api.execute(name, args.to_serde()?, &ctx.context).await?;
                if matches!(name, "link" | "share" | "remove" | "join") {
                    api.publish(&sessions, &snapshot, &ctx.context).await?;
                }
                Ok(wire(value))
            })
        })?;
    }
    Ok(members)
}

fn presence_members(object: &mut ObjectSelf) -> Result<Members> {
    let references = object.host().references("p2.guest-presence");
    let source = object.follow(move |target, ctx| {
        Box::pin(async move {
            let update = |references: Vec<pi_rs_chord::host::Presence>| -> Result<JsonValue> {
                let mut guests = IndexMap::new();
                for reference in references {
                    if let Some(principal) = reference.principal
                        && let Principal::Guest {
                            id, name, sessions, ..
                        } = serde_json::from_value(principal)?
                    {
                        guests.insert(id.clone(), json!({"id":id,"name":name,"sessions":sessions}));
                    }
                }
                Ok(wire(serde_json::to_value(guests)?))
            };
            let changes = target.clone();
            let observe = references.observe(
                &ctx.context,
                move |change, ctx| {
                    if let NativeWatchCall::Value { value } = change {
                        changes.set(update(value)?, ctx)?;
                    }
                    Ok(())
                },
                |_| {},
            )?;
            target.set(update(references.value())?, &ctx.context)?;
            Ok(Box::new(move || drop(observe)) as Box<dyn FnOnce() + Send>)
        })
    })?;
    let mut members = Members::new();
    members.state("guests", source)?;
    Ok(members)
}

fn sharing_members(
    params: &JsonValue,
    principal: Option<&Value>,
    object: &mut ObjectSelf,
) -> Result<Members> {
    if !principal.is_some_and(|p| p["kind"] == "owner") {
        return Err(ChordError::new("forbidden", "sharing is owner-only").into());
    }
    let params = params.to_serde()?;
    let session = arg(&params, "session")?.to_owned();
    let catalog = object.get(Arc::new(lookup("p2.catalog")?), wire(json!({})))?;
    let sessions = catalog.member("sessions")?;
    let shares = object.get(Arc::new(lookup(OWNER)?), wire(json!({})))?;
    let presence = object.get(Arc::new(lookup("p2.share-presence")?), wire(json!({})))?;
    let natives = server_of(&object.host())?;
    let origin = natives.info.allowed_hosts.first().map_or_else(
        || format!("http://127.0.0.1:{}", natives.info.port),
        |host| format!("https://{host}"),
    );
    let mut members = Members::variant("owner");
    let id = session.clone();
    members.state(
        "links",
        derive(object, shares.member("links")?, move |value| {
            let links: Vec<Link> = serde_json::from_value(value)?;
            Ok(json!(
                links
                    .into_iter()
                    .filter(|link| link.session == id)
                    .map(|link| {
                        let url = format!("{origin}/join/{}", link.token);
                        json!({"mode":link.mode,"token":link.token,"url":url})
                    })
                    .collect::<Vec<_>>()
            ))
        }),
    )?;
    let id = session.clone();
    members.state("guests", derive(object, presence.member("guests")?, move |value| {
        let guests: IndexMap<String, Value> = serde_json::from_value(value)?;
        let mut result: Vec<_> = guests.into_values().filter(|guest| guest["sessions"].get(&id).is_some())
            .map(|guest| json!({"id":guest["id"],"name":guest["name"],"mode":guest["sessions"][&id]})).collect();
        result.sort_by(|a,b| a["name"].as_str().cmp(&b["name"].as_str()));
        Ok(json!(result))
    }))?;
    let id = session.clone();
    members.state(
        "people",
        derive(object, shares.member("people")?, move |value| {
            let people: Vec<Value> = serde_json::from_value(value)?;
            Ok(
                json!(people.into_iter().filter(|person| person["session"] == id)
            .map(|person| json!({"id":person["id"],"name":person["name"],"mode":person["mode"]}))
            .collect::<Vec<_>>()),
            )
        }),
    )?;
    let mut env_inputs = vec![sessions];
    if object.host().served_contract("p2.child-store").is_some() {
        let children = object.get(Arc::new(lookup("p2.child-store")?), wire(json!({})))?;
        env_inputs.push(children.member("children")?);
    }
    let id = session.clone();
    members.state(
        "env",
        Source::derive(
            object.batch(),
            env_inputs,
            move |versions| {
                let rows: Vec<Value> = serde_json::from_value(versions[0].to_json().to_serde()?)?;
                let row = rows
                    .iter()
                    .find(|row| row["id"] == id)
                    .unwrap_or(&Value::Null);
                let children = versions
                    .get(1)
                    .map(|v| v.to_json().to_serde())
                    .transpose()?
                    .unwrap_or(Value::Null);
                let env = environment_names(row, &children[&id]);
                Ok(Revision::from_json(wire(if env.is_empty() {
                    Value::Null
                } else {
                    json!(env.join(", "))
                }))?)
            },
            |_| {},
        ),
    )?;
    let people_url =
        crate::api::v1::server::service_of::<PeopleUrl>(&object.host(), "p2.share-urls")
            .ok()
            .and_then(|service| (service.0)(&session));
    members.state("peopleUrl", object.writable(wire(json!(people_url)))?)?;
    for method in ["link", "share", "remove"] {
        let shares: Held = shares.clone();
        let id = session.clone();
        members.method(method, move |args, ctx| {
            let shares = shares.clone();
            let id = id.clone();
            Box::pin(async move {
                let mut args = args.to_serde()?;
                args["session"] = json!(id);
                if method == "link" && args.get("reset").is_none() {
                    args["reset"] = json!(false);
                }
                shares.call(method, wire(args), &ctx.context).await
            })
        })?;
    }
    Ok(members)
}

type ResolvePeopleUrl = dyn Fn(&str) -> Option<String> + Send + Sync;

pub struct PeopleUrl(pub Box<ResolvePeopleUrl>);

pub(super) fn facet(tables: Arc<Tables>) -> Result<Facet> {
    let shares = Arc::new(lookup(OWNER)?);
    let sharing = Arc::new(lookup("p2.sharing")?);
    let guest = Arc::new(lookup("p2.guest-presence")?);
    let presence = Arc::new(lookup("p2.share-presence")?);
    Ok(define_facet(move |host| {
        let tables = tables.clone();
        host.serve(
            shares.clone(),
            move |_, _, object| shares_members(object, tables.clone()),
            ServeOptions::default(),
        )?;
        host.serve(sharing.clone(), sharing_members, ServeOptions::default())?;
        host.serve(
            presence.clone(),
            |_, _, object| presence_members(object),
            ServeOptions::default(),
        )?;
        host.serve(
            guest.clone(),
            |_, principal, _| match principal.and_then(|p| p["kind"].as_str()) {
                Some(kind @ ("guest" | "owner")) => Ok(Members::variant(kind)),
                _ => Err(ChordError::new("forbidden", "owners and guests only").into()),
            },
            ServeOptions::default(),
        )?;
        host.contribute_http(http::routes(host.host().downgrade(), shares.clone())?)
    }))
}
