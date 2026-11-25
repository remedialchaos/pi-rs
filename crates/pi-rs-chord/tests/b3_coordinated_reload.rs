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
    node::plugins::{
        PluginRuntime, PluginScript, ReloadCoordinator, RolloutStep, coordinated_rollout,
    },
};
use std::{sync::Arc, time::Duration};

const V1: &[u8] = include_bytes!("fixtures/b3/v1.wasm");
const REJECT: &[u8] = include_bytes!("fixtures/b3/reject.wasm");
const DOUBLE: &[u8] = include_bytes!("fixtures/b3/double.wasm");

fn counter(bytes: &[u8]) -> PluginScript {
    PluginScript::wasm("counter", bytes.to_vec(), true)
}

#[tokio::test]
async fn coordinated_rollout_holds_back_all_hosts_on_failure_and_commits_when_all_stage()
-> Result<()> {
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
    let host_a = create_host(HostOptions::default())?;
    let host_b = create_host(HostOptions::default())?;
    let v1 = counter(V1);
    let (runtime_a, load_a) =
        PluginRuntime::load(host_a.clone(), vec![], vec![v1.clone()], &ctx).await?;
    let (runtime_b, load_b) =
        PluginRuntime::load(host_b.clone(), vec![], vec![v1.clone()], &ctx).await?;
    assert_eq!(load_a.added, ["counter"]);
    assert_eq!(load_b.added, ["counter"]);

    let params = JsonValue::Object(Default::default());
    let ref_a = host_a.hold(contract.clone(), params.clone(), &ctx)?;
    let ref_b = host_b.hold(contract, params, &ctx)?;
    let wait_for_guests = || async {
        tokio::time::timeout(Duration::from_secs(5), async {
            ref_a.wait(&ctx).await?;
            ref_b.wait(&ctx).await
        })
        .await
        .map_err(|e| Error::Invalid(format!("guest readiness: {e}")))?
    };
    wait_for_guests().await?;

    assert_eq!(
        ref_a.call("add", JsonValue::Number(7.0), &ctx).await?,
        JsonValue::Number(7.0)
    );
    assert_eq!(ref_a.read("total")?, JsonValue::Number(7.0));
    assert_eq!(
        ref_b.call("add", JsonValue::Number(3.0), &ctx).await?,
        JsonValue::Number(3.0)
    );
    assert_eq!(ref_b.read("total")?, JsonValue::Number(3.0));

    // Phase 1: host_a has valid DOUBLE, host_b has REJECT -> both must hold back on v1.
    let double = counter(DOUBLE);
    let held_back = coordinated_rollout(
        vec![
            RolloutStep {
                name: "host_a".into(),
                runtime: &runtime_a,
                scripts: vec![double.clone()],
            },
            RolloutStep {
                name: "host_b".into(),
                runtime: &runtime_b,
                scripts: vec![counter(REJECT)],
            },
        ],
        &ctx,
    )
    .await?;
    assert!(!held_back.committed, "rollout must abort when host_b fails");
    assert_eq!(held_back.participants.len(), 2);
    assert!(held_back.participants[0].1.failed.is_empty());
    assert_eq!(held_back.participants[1].1.failed.len(), 1);
    assert!(
        held_back.participants[1].1.failed[0]
            .error
            .contains("fixture rejected setup")
    );
    assert_eq!(runtime_a.digest("counter").as_deref(), Some(v1.digest()));
    assert_eq!(runtime_b.digest("counter").as_deref(), Some(v1.digest()));

    // Both hosts must still serve their live v1 instances with preserved state.
    wait_for_guests().await?;
    assert_eq!(ref_a.read("total")?, JsonValue::Number(7.0));
    assert_eq!(
        ref_a.call("add", JsonValue::Number(2.0), &ctx).await?,
        JsonValue::Number(9.0)
    );
    assert_eq!(ref_b.read("total")?, JsonValue::Number(3.0));
    assert_eq!(
        ref_b.call("add", JsonValue::Number(2.0), &ctx).await?,
        JsonValue::Number(5.0)
    );

    // Phase 2: both hosts stage DOUBLE -> both commit atomically.
    let committed = ReloadCoordinator::new()
        .add("host_a", &runtime_a, vec![double.clone()])
        .add("host_b", &runtime_b, vec![double.clone()])
        .execute(&ctx)
        .await?;
    assert!(committed.committed);
    assert_eq!(committed.participants[0].1.changed, ["counter"]);
    assert_eq!(committed.participants[1].1.changed, ["counter"]);
    assert_eq!(
        runtime_a.digest("counter").as_deref(),
        Some(double.digest())
    );
    assert_eq!(
        runtime_b.digest("counter").as_deref(),
        Some(double.digest())
    );

    wait_for_guests().await?;
    assert_eq!(
        ref_a.call("add", JsonValue::Number(3.0), &ctx).await?,
        JsonValue::Number(6.0)
    );
    assert_eq!(
        ref_b.call("add", JsonValue::Number(4.0), &ctx).await?,
        JsonValue::Number(8.0)
    );

    // Duplicate participant must be rejected rather than deadlocking.
    assert!(
        coordinated_rollout(
            vec![
                RolloutStep {
                    name: "dup1".into(),
                    runtime: &runtime_a,
                    scripts: vec![double.clone()],
                },
                RolloutStep {
                    name: "dup2".into(),
                    runtime: &runtime_a,
                    scripts: vec![double],
                },
            ],
            &ctx,
        )
        .await
        .is_err()
    );

    host_a.close(&ctx).await?;
    host_b.close(&ctx).await?;
    Ok(())
}
