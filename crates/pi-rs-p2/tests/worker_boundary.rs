use std::{error::Error as StdError, sync::Arc, time::Duration};

use pi_rs_chord::{
    Error,
    context::Context,
    host::{HostOptions, PluginFacet, create_host},
    json::JsonValue,
    link::memory_link_pair,
    remote::{RemoteOptions, RemotePeer},
    state::Batch,
    sync::SyncFuture,
};
use pi_rs_p2::api::v1::{
    session_address,
    worker::{
        Location, WorkerBoundary, WorkerBoundaryBackend, WorkerInspection, worker_boundary_contract,
    },
};
use serde_json::json;

struct Backend;

impl WorkerBoundaryBackend for Backend {
    fn inspect(&self, _: Context) -> SyncFuture<WorkerInspection> {
        Box::pin(async {
            Ok(WorkerInspection {
                tasks: 0,
                busy: vec![],
            })
        })
    }

    fn check_relocation(
        &self,
        _: Location,
        to: Location,
        _: Context,
    ) -> SyncFuture<Option<String>> {
        Box::pin(async move { Ok((to.cwd == "/refused").then(|| "refused by backend".into())) })
    }

    fn relocate(&self, location: Location, _: Context) -> SyncFuture<()> {
        Box::pin(async move {
            if location.cwd == "/failed" {
                return Err(Error::Invalid("move failed".into()));
            }
            Ok(())
        })
    }
}

#[tokio::test]
async fn worker_boundary_admits_only_server_and_preserves_a_failed_relocation_fence()
-> Result<(), Box<dyn StdError>> {
    let context = Context::background();
    let initial = Location {
        cwd: "/old".into(),
        env: None,
    };
    let boundary = Arc::new(WorkerBoundary::new(initial.clone(), Arc::new(Backend)));
    let contract = worker_boundary_contract()?;
    let params = JsonValue::from_serde(json!({"session":"active"}));
    let host = create_host(HostOptions {
        address: Some(JsonValue::from_serde(session_address("active"))),
        principal: Some(json!({"kind":"host"})),
        ..Default::default()
    })?;
    let ready = host
        .load(
            vec![PluginFacet::new(
                "@p2/core",
                boundary.facet("active".into())?,
            )],
            &context,
        )
        .await?;
    assert!(ready.failed.is_empty());

    let (local, remote) = memory_link_pair();
    host.accept(
        local,
        RemoteOptions::new(json!({"kind":"owner","access":"local"})),
    )?;
    let guest = RemotePeer::connect(
        remote,
        RemoteOptions::new(json!({"kind":"host"})),
        Batch::new(|_| {}),
    )?;
    let denied = guest.hold(contract.clone(), params.clone(), &context)?;
    assert!(
        tokio::time::timeout(Duration::from_secs(3), denied.wait(&context))
            .await?
            .is_err()
    );

    let (local, remote) = memory_link_pair();
    host.accept(local, RemoteOptions::new(json!({"kind":"server"})))?;
    let server = RemotePeer::connect(
        remote,
        RemoteOptions::new(json!({"kind":"host"})),
        Batch::new(|_| {}),
    )?;
    let reference = server.hold(contract, params, &context)?;
    tokio::time::timeout(Duration::from_secs(3), async {
        let location = boundary.location();
        let refused = reference
            .call(
                "prepare",
                JsonValue::from_serde(json!({"to":{"cwd":"/refused","env":null}})),
                &context,
            )
            .await?;
        assert_eq!(
            refused.to_serde()?,
            json!({"accepted":false,"tasks":0,"busy":[],"refusal":"refused by backend"})
        );
        assert!(!boundary.liveness().relocating());

        let prepared = reference.call("prepare", JsonValue::Null, &context).await?;
        assert_eq!(
            prepared.to_serde()?,
            json!({"accepted":true,"tasks":0,"busy":[]})
        );
        assert!(boundary.liveness().call(async { Ok(()) }).await.is_err());

        let failed = reference
            .call(
                "relocate",
                JsonValue::from_serde(json!({"cwd":"/failed","env":null})),
                &context,
            )
            .await;
        assert!(failed.is_err());
        assert!(boundary.liveness().relocating());
        assert_eq!(*location.borrow(), (0, initial));

        reference.call("cancel", JsonValue::Null, &context).await?;
        assert!(!boundary.liveness().relocating());
        reference.call("prepare", JsonValue::Null, &context).await?;
        let destination = Location {
            cwd: "/new".into(),
            env: Some("box".into()),
        };
        reference
            .call(
                "relocate",
                JsonValue::from_serde(serde_json::to_value(&destination)?),
                &context,
            )
            .await?;
        assert_eq!(*location.borrow(), (1, destination));
        assert!(!boundary.liveness().relocating());
        Ok::<(), Error>(())
    })
    .await??;
    server.close();
    guest.close();
    host.close(&context).await?;
    Ok(())
}
