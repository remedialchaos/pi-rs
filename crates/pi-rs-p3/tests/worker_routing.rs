use std::{sync::Arc, time::Duration};

use pi_rs_chord::{
    context::Context,
    json::JsonValue,
    node::http::connect_websocket,
    remote::{RemoteOptions, RemotePeer},
    state::Batch,
};
use pi_rs_p3::{
    install::WorkerSpec,
    plugins::{
        sessions::{Creator, SessionRow, SessionState, session_worker_contract},
        sign_in::{SignInKind, owner_token_file},
    },
    principals::Principal,
    server::{ServerOptions, start_server},
};
use serde_json::json;

#[tokio::test]
async fn chord_routes_session_holds_to_child_workers_and_enforces_principal_and_session_isolation()
{
    let root = tempfile::tempdir().expect("temp dir");
    let options = ServerOptions {
        data_dir: root.path().to_path_buf(),
        port: 0,
        worker: WorkerSpec::at(env!("CARGO_BIN_EXE_p3")),
        worker_idle_ms: 60_000,
        version: "test".into(),
        on_stop: Arc::new(|| {}),
        on_error: Arc::new(|err| eprintln!("server error: {err}")),
    };
    let ctx = Context::background();
    let server = start_server(options, &ctx).await.expect("start server");

    let s1 = server
        .runtime
        .sessions_store()
        .create("first", Creator::Owner)
        .expect("create s1");
    let s2 = server
        .runtime
        .sessions_store()
        .create("second", Creator::Owner)
        .expect("create s2");

    let owner_token = std::fs::read_to_string(owner_token_file(&server.data_dir))
        .expect("owner token")
        .trim()
        .to_string();
    let person_issued = server
        .runtime
        .sign_ins()
        .issue(
            &Principal::Person {
                id: "p1".into(),
                name: "Guest".into(),
            },
            "guest",
            SignInKind::Device,
            false,
        )
        .expect("issue person token");

    let ws_url = format!("ws://127.0.0.1:{}/chord", server.port);

    // 1. Unauthenticated WebSocket link must be rejected.
    assert!(
        connect_websocket(
            &ws_url,
            http::HeaderMap::new(),
            1024 * 1024,
            8 * 1024 * 1024
        )
        .await
        .is_err()
    );

    // 2. Owner link routes session-scoped holds directly to distinct child worker processes.
    let mut owner_headers = http::HeaderMap::new();
    owner_headers.insert(
        http::header::AUTHORIZATION,
        http::HeaderValue::from_str(&format!("Bearer {owner_token}")).expect("auth header"),
    );
    let owner_link = connect_websocket(&ws_url, owner_headers, 1024 * 1024, 8 * 1024 * 1024)
        .await
        .expect("owner ws link");
    let owner_peer = RemotePeer::connect(
        owner_link.clone(),
        RemoteOptions::new(Principal::Server.value()),
        Batch::new(|_| {}),
    )
    .expect("owner peer");

    let contract = session_worker_contract().expect("contract");
    let hold_s1 = owner_peer
        .hold(
            contract.clone(),
            JsonValue::from_serde(json!({ "id": s1 })),
            &ctx,
        )
        .expect("hold s1");
    let hold_s2 = owner_peer
        .hold(
            contract.clone(),
            JsonValue::from_serde(json!({ "id": s2 })),
            &ctx,
        )
        .expect("hold s2");

    tokio::time::timeout(Duration::from_secs(15), async {
        hold_s1.wait(&ctx).await?;
        hold_s2.wait(&ctx).await
    })
    .await
    .expect("worker holds ready timeout")
    .expect("worker holds ready");

    let pid_s1 = hold_s1
        .member("pid")
        .expect("pid member")
        .value()
        .expect("pid value")
        .to_json()
        .to_serde()
        .expect("pid json")
        .as_i64()
        .expect("pid i64");
    let pid_s2 = hold_s2
        .member("pid")
        .expect("pid member")
        .value()
        .expect("pid value")
        .to_json()
        .to_serde()
        .expect("pid json")
        .as_i64()
        .expect("pid i64");
    assert_ne!(pid_s1, i64::from(std::process::id()));
    assert_ne!(pid_s2, i64::from(std::process::id()));
    assert_ne!(
        pid_s1, pid_s2,
        "each session must run in its own child worker"
    );

    let info_s1 = hold_s1
        .call("info", JsonValue::Null, &ctx)
        .await
        .expect("info s1")
        .to_serde()
        .expect("serde");
    assert_eq!(
        info_s1,
        json!({
            "session": s1,
            "pid": pid_s1,
            "caller": { "kind": "owner" },
        })
    );
    let info_s2 = hold_s2
        .call("info", JsonValue::Null, &ctx)
        .await
        .expect("info s2")
        .to_serde()
        .expect("serde");
    assert_eq!(info_s2["session"], s2);
    assert_eq!(info_s2["pid"], pid_s2);

    // 3. Worker-to-server bidirectional link: s1 worker fetches its own SessionRow and reports state.
    let own_facts: SessionRow = serde_json::from_value(
        hold_s1
            .call("facts", JsonValue::Null, &ctx)
            .await
            .expect("own facts")
            .to_serde()
            .expect("serde"),
    )
    .expect("SessionRow");
    assert_eq!(own_facts.id, s1);
    assert_eq!(own_facts.title, "first");

    hold_s1
        .call(
            "report",
            JsonValue::from_serde(json!({ "state": "working" })),
            &ctx,
        )
        .await
        .expect("report working");
    assert_eq!(
        server
            .runtime
            .sessions_store()
            .one(&s1)
            .expect("s1 row")
            .state,
        SessionState::Working
    );

    // 4. Cross-session isolation: s1's worker asking server for s2's facts must be rejected!
    assert!(
        hold_s1
            .call("facts", JsonValue::from_serde(json!({ "id": s2 })), &ctx)
            .await
            .is_err(),
        "s1 worker must not read s2's session facts"
    );

    // 5. Missing session must be rejected without spawning a worker.
    let missing = owner_peer
        .hold(
            contract.clone(),
            JsonValue::from_serde(json!({ "id": "no-such-session" })),
            &ctx,
        )
        .expect("hold missing");
    assert!(
        tokio::time::timeout(Duration::from_secs(5), missing.wait(&ctx))
            .await
            .expect("missing wait timeout")
            .is_err(),
        "non-existent session must be rejected"
    );
    missing.release();

    // 6. Unauthorized principal (Person) must be rejected from holding a session worker.
    let mut person_headers = http::HeaderMap::new();
    person_headers.insert(
        http::header::AUTHORIZATION,
        http::HeaderValue::from_str(&format!("Bearer {}", person_issued.token))
            .expect("person auth header"),
    );
    let person_link = connect_websocket(&ws_url, person_headers, 1024 * 1024, 8 * 1024 * 1024)
        .await
        .expect("person ws link");
    let person_peer = RemotePeer::connect(
        person_link.clone(),
        RemoteOptions::new(Principal::Server.value()),
        Batch::new(|_| {}),
    )
    .expect("person peer");
    let denied = person_peer
        .hold(contract, JsonValue::from_serde(json!({ "id": s1 })), &ctx)
        .expect("hold denied");
    assert!(
        tokio::time::timeout(Duration::from_secs(5), denied.wait(&ctx))
            .await
            .expect("denied wait timeout")
            .is_err(),
        "non-owner principal must be denied access to session worker"
    );
    denied.release();
    person_peer.close();

    // 7. Worker-to-worker admission must also be denied.
    assert!(matches!(
        server
            .runtime
            .sessions()
            .admit(&Principal::worker(&s1), &s2),
        pi_rs_p3::plugins::sessions::Admission::Deny(_)
    ));

    hold_s1.release();
    hold_s2.release();
    owner_peer.close();
    server.close(&ctx).await.expect("close server");
}
