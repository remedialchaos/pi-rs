use indexmap::IndexMap;
use pi_rs_chord::{
    Error, Result,
    context::Context,
    contracts::{
        Contract, Member,
        schema::{int, method},
    },
    host::{HostOptions, create_host},
    json::JsonValue,
    node::plugins::{PluginRuntime, PluginScript, RolloutStep, rollout},
};
use std::{sync::Arc, time::Duration};

const V1: &[u8] = include_bytes!("fixtures/b3/v1.wasm");
const REJECT: &[u8] = include_bytes!("fixtures/b3/reject.wasm");
const DOUBLE: &[u8] = include_bytes!("fixtures/b3/double.wasm");

fn counter(bytes: &[u8]) -> PluginScript {
    PluginScript::wasm("counter", bytes.to_vec(), true)
}

#[tokio::test]
async fn compiled_guest_publishes_state_and_rollout_retains_a_rejected_host() -> Result<()> {
    let ctx = Context::background();
    let contract = Arc::new(Contract::local(
        "guest-counter",
        1,
        vec![],
        IndexMap::from([
            ("total".into(), Member::State(int())),
            ("add".into(), method(Some(int()), Some(int()))),
        ]),
    )?);
    let server = create_host(HostOptions::default())?;
    let worker = create_host(HostOptions::default())?;
    let v1 = counter(V1);
    let (server_runtime, server_load) =
        PluginRuntime::load(server.clone(), vec![], vec![v1.clone()], &ctx).await?;
    let (worker_runtime, worker_load) =
        PluginRuntime::load(worker.clone(), vec![], vec![v1.clone()], &ctx).await?;
    assert_eq!(server_load.added, ["counter"]);
    assert_eq!(worker_load.added, ["counter"]);
    let params = JsonValue::Object(Default::default());
    let server_ref = server.hold(contract.clone(), params.clone(), &ctx)?;
    let worker_ref = worker.hold(contract, params, &ctx)?;
    let wait_for_guests = || async {
        tokio::time::timeout(Duration::from_secs(5), async {
            server_ref.wait(&ctx).await?;
            worker_ref.wait(&ctx).await
        })
        .await
        .map_err(|e| Error::Invalid(format!("guest readiness: {e}")))?
    };
    wait_for_guests().await?;
    assert_eq!(server_ref.read("total")?, JsonValue::Number(0.0));
    assert_eq!(
        server_ref.call("add", JsonValue::Number(7.0), &ctx).await?,
        JsonValue::Number(7.0)
    );
    assert_eq!(server_ref.read("total")?, JsonValue::Number(7.0));
    assert_eq!(
        worker_ref.call("add", JsonValue::Number(3.0), &ctx).await?,
        JsonValue::Number(3.0)
    );
    assert_eq!(worker_ref.read("total")?, JsonValue::Number(3.0));

    let successor = counter(DOUBLE);
    let reports = rollout(
        vec![
            RolloutStep {
                name: "server".into(),
                runtime: &server_runtime,
                scripts: vec![counter(REJECT)],
            },
            RolloutStep {
                name: "worker".into(),
                runtime: &worker_runtime,
                scripts: vec![successor.clone()],
            },
        ],
        &ctx,
    )
    .await?;
    let [(server_name, held), (worker_name, updated)] = reports.as_slice() else {
        return Err(Error::Invalid(format!(
            "expected two rollout reports: {reports:?}"
        )));
    };
    assert_eq!(server_name, "server");
    assert_eq!(worker_name, "worker");
    assert!(held.changed.is_empty());
    assert_eq!(held.failed.len(), 1);
    assert_eq!(held.failed[0].plugin, "counter");
    assert!(
        held.failed[0].error.contains("fixture rejected setup"),
        "{held:?}"
    );
    assert!(updated.failed.is_empty(), "{updated:?}");
    assert_eq!(updated.changed, ["counter"]);
    assert_eq!(
        server_runtime.digest("counter").as_deref(),
        Some(v1.digest())
    );
    assert_eq!(
        worker_runtime.digest("counter").as_deref(),
        Some(successor.digest())
    );

    wait_for_guests().await?;
    assert_eq!(server_ref.read("total")?, JsonValue::Number(7.0));
    assert_eq!(
        server_ref.call("add", JsonValue::Number(2.0), &ctx).await?,
        JsonValue::Number(9.0)
    );
    assert_eq!(server_ref.read("total")?, JsonValue::Number(9.0));
    assert_eq!(worker_ref.read("total")?, JsonValue::Number(0.0));
    assert_eq!(
        worker_ref.call("add", JsonValue::Number(2.0), &ctx).await?,
        JsonValue::Number(4.0)
    );
    assert_eq!(worker_ref.read("total")?, JsonValue::Number(4.0));
    server.close(&ctx).await?;
    worker.close(&ctx).await?;
    Ok(())
}
