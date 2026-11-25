//! Register the relay's native services and resolve relayed requests before local admission.

use std::{
    path::Path,
    sync::{Arc, Weak},
};

use pi_rs_chord::{
    Error, Result,
    host::{Facet, Members, ObjectSelf, ServeOptions},
    state::Source,
};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

use crate::{
    api::v1::{
        OwnerAccess, Principal,
        p2_contracts::lookup,
        server::{install_service, server_of, service_of},
    },
    plugins::{
        shard_a::PeopleUrl,
        sync::{RadiusSyncAccess, RadiusSyncError, RadiusSyncRemote, is_terminal_sync_error},
    },
    runtime::server::{RelayUser, ResolvedRequest, ServerServices},
};

use super::{
    brain::{RadiusBrain, Status},
    relay::RelayAccess,
    tunnel::ResolveForward,
};

pub const CONNECTIONS: &str = "p2.radius-connections";
pub const BACKUP_TARGET: &str = "p2.radius-backup-target";
pub const FORWARD_RESOLVER: &str = "p2.radius-forward-resolver";

pub struct RadiusConnection {
    pub relay: RelayAccess,
    pub brain: Arc<RadiusBrain>,
}

pub struct RadiusBackupTarget(Weak<RadiusBrain>);

pub struct RadiusBackup {
    pub remote: RadiusSyncRemote,
    pub device: String,
    pub server: String,
    pub terminal: fn(&RadiusSyncError) -> bool,
}

impl RadiusBackupTarget {
    pub async fn open(&self) -> Result<RadiusBackup> {
        let brain = self.0.upgrade().ok_or(Error::Closed)?;
        let access = brain.sync_access().await?;
        let weak = self.0.clone();
        Ok(RadiusBackup {
            remote: RadiusSyncRemote::new(Arc::new(move || {
                let weak = weak.clone();
                Box::pin(async move {
                    let brain = weak.upgrade().ok_or(Error::Closed)?;
                    let access = brain.sync_access().await?;
                    Ok(RadiusSyncAccess {
                        gateway: access.gateway,
                        server_id: access.server_id,
                        token: access.token,
                    })
                })
            })),
            server: format!("server-{}", access.server_id),
            device: access.server_id,
            terminal: is_terminal_sync_error,
        })
    }
}

struct Service {
    connection: Arc<RadiusConnection>,
    _share: Arc<PeopleUrl>,
    _backup: Arc<RadiusBackupTarget>,
    relay_task: Option<tokio::task::JoinHandle<()>>,
}

pub(super) fn facet() -> Result<Facet> {
    let contract = Arc::new(lookup("p2.radius-service")?);
    let relay = RelayAccess::new();
    let mut services = ServerServices::default();
    let access = relay.clone();
    services.request_resolvers.insert(
        "radius".into(),
        Arc::new(move |headers| {
            Ok(access.verify("127.0.0.1", headers).map(|relayed| {
                let principal = relayed.user.is_none().then_some(Principal::Owner {
                    access: OwnerAccess::Relay,
                });
                ResolvedRequest {
                    origin: relayed.public_origin,
                    shell_origin: relayed.shell_origin,
                    user: relayed.user.map(|user| RelayUser {
                        id: user.id,
                        email: user.email,
                    }),
                    principal,
                }
            }))
        }),
    );
    Ok(services.with_facet(move |host| {
        let relay = relay.clone();
        host.serve(
            contract.clone(),
            move |_, _, object| members(object, relay.clone()),
            ServeOptions::default(),
        )
    }))
}

fn peer(object: &mut ObjectSelf, name: &str, member: &str) -> Result<Option<Source>> {
    object
        .host()
        .served_contract(name)
        .map(|contract| object.get(contract, json!({}).into())?.member(member))
        .transpose()
}

fn members(object: &mut ObjectSelf, relay: RelayAccess) -> Result<Members> {
    let mut name = peer(object, "p2.server-name", "name")?;
    let mut shares = peer(object, "p2.shares", "emails")?;
    let host = object.host().downgrade();
    let service = object.open(
        move |ctx| {
            Box::pin(async move {
                let host = host.upgrade().ok_or(Error::Closed)?;
                for (source, contract_name, member) in [
                    (&mut name, "p2.server-name", "name"),
                    (&mut shares, "p2.shares", "emails"),
                ] {
                    if source.is_none()
                        && let Some(contract) = host.served_contract(contract_name)
                    {
                        *source = Some(ctx.get(contract, json!({}).into())?.member(member)?);
                    }
                }
                let server = server_of(&host)?;
                let weak_host = host.downgrade();
                let forward: ResolveForward = Arc::new(move |id| {
                    let host = weak_host.upgrade()?;
                    let resolver = service_of::<ResolveForward>(&host, FORWARD_RESOLVER).ok()?;
                    resolver(id)
                });
                let brain = Arc::new(RadiusBrain::new(
                    Path::new(&server.info.data_dir),
                    server.info.port,
                    relay.clone(),
                    forward,
                )?);
                let connection = Arc::new(RadiusConnection {
                    relay,
                    brain: brain.clone(),
                });
                install_service(&host, CONNECTIONS, &connection)?;
                let weak = Arc::downgrade(&brain);
                let share = Arc::new(PeopleUrl(Box::new(move |session| {
                    let mut url = url::Url::parse(&weak.upgrade()?.shell_page()?).ok()?;
                    url.path_segments_mut()
                        .ok()?
                        .pop_if_empty()
                        .push("s")
                        .push(session);
                    Some(url.into())
                })));
                install_service(&host, "p2.share-urls", &share)?;
                let backup = Arc::new(RadiusBackupTarget(Arc::downgrade(&brain)));
                install_service(&host, BACKUP_TARGET, &backup)?;
                if let Ok(targets) = crate::plugins::backup::get_or_install_backup_targets(&host) {
                    let radius_target = backup.clone();
                    let _ = targets.offer(
                        "radius",
                        Arc::new(move || {
                            let radius_target = radius_target.clone();
                            Box::pin(async move {
                                let opened = radius_target.open().await?;
                                Ok(crate::plugins::backup::OpenBackupTarget {
                                    remote: Arc::new(opened.remote),
                                    device: opened.device,
                                    server: opened.server,
                                    terminal: None,
                                })
                            })
                        }),
                    );
                }
                let relay_task = if std::env::var("P2_RADIUS").as_deref() == Ok("0") {
                    None
                } else {
                    let brain = brain.clone();
                    Some(tokio::spawn(async move { run(brain, name, shares).await }))
                };
                Ok(Service {
                    connection,
                    _share: share,
                    _backup: backup,
                    relay_task,
                })
            })
        },
        |service, _| {
            Box::pin(async move {
                service.connection.brain.close();
                if let Some(task) = &service.relay_task {
                    task.abort();
                }
                Ok(())
            })
        },
    );
    // The object owns its native service until the facet closes.
    drop(service);
    Ok(Members::new())
}

async fn run(brain: Arc<RadiusBrain>, name: Option<Source>, shares: Option<Source>) {
    let (updated, mut updates) = mpsc::channel::<()>(1);
    let _subscriptions: Vec<_> = [&name, &shares]
        .into_iter()
        .filter_map(Option::as_ref)
        .map(|source| {
            let updated = updated.clone();
            source.subscribe(
                move |_, _| {
                    let _ = updated.try_send(());
                    Ok(())
                },
                |_| {},
            )
        })
        .collect();
    follow(&brain, name.as_ref(), shares.as_ref());
    let mut status = brain.status();
    let task = brain.start();
    let mut last = String::new();
    loop {
        tokio::select! {
            result = updates.recv() => {
                if result.is_none() { break; }
                follow(&brain,name.as_ref(),shares.as_ref());
            }
            result = status.changed() => {
                if result.is_err() { break; }
                print_status(&mut last,&mut status);
            }
        }
    }
    task.abort();
}

fn follow(brain: &RadiusBrain, name: Option<&Source>, shares: Option<&Source>) {
    let read = |source: Option<&Source>| -> Option<Value> {
        source?.value().ok()?.to_json().to_serde().ok()
    };
    brain.rename(read(name).and_then(|value| value.as_str().map(str::to_owned)));
    if let Some(shares) =
        read(shares).and_then(|value| serde_json::from_value::<Vec<String>>(value).ok())
    {
        brain.report_shares(shares);
    }
}

fn print_status(last: &mut String, status: &mut watch::Receiver<Status>) {
    let text = match &*status.borrow_and_update() {
        Status::Connected {
            server_id, name, ..
        } => match name {
            Some(name) => format!("connected as brain \"{name}\" ({server_id})"),
            None => format!("connected as brain {server_id}"),
        },
        Status::Retrying { error } => format!("not connected ({error})"),
        Status::SignedOut | Status::Connecting => return,
    };
    if text != *last {
        println!("Radius: {text}");
        *last = text;
    }
}
