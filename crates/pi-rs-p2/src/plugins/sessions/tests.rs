use super::*;
use crate::hosts::server::{P2Server, ServerOptions, start_server};
use pi_rs_chord::{
    context::Context,
    host::{Caller, Held},
};
use std::path::Path;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

async fn start(path: &Path) -> TestResult<P2Server> {
    let mut options = ServerOptions::new(path.to_owned());
    options.port = 0;
    options.dev = false;
    options.fake = true;
    super::register(&mut options.registry)?;
    Ok(start_server(options).await?)
}

async fn call(held: &Held, name: &str, args: Value, context: &Context) -> Result<Value> {
    held.call(name, wire(args), context).await?.to_serde()
}

#[tokio::test]
async fn explicit_titles_and_tree_survive_activity_and_restart() -> TestResult {
    let directory = tempfile::tempdir()?;
    let server = start(directory.path()).await?;
    let contract = Arc::new(lookup("p2.catalog")?);
    let context = Context::background();
    let catalog = server
        .host
        .get(contract.clone(), wire(json!({})), &context)
        .await?;
    let id = uuid::Uuid::new_v4().to_string();
    let cwd = directory
        .path()
        .to_str()
        .ok_or("temporary path not UTF-8")?;
    let args = json!({
        "id":id, "title":"New session", "cwd":cwd,
        "model":{"provider":"fake","modelId":"faux-1"}, "thinkingLevel":"off",
    });
    let created = call(&catalog, "create", args.clone(), &context).await?;
    assert!(matches!(
        call(&catalog, "create", args, &context).await,
        Err(Error::Chord(error)) if error.code == "conflict"
    ));
    call(
        &catalog,
        "activity",
        json!({
            "id":id, "timestamp":1, "firstUserText":"first\nmessage"
        }),
        &context,
    )
    .await?;
    let before = call(&catalog, "get", json!({"id":id}), &context).await?;
    assert_eq!(before["title"], "first message");
    assert_eq!(before["updatedAt"], created["updatedAt"]);
    let branches = json!([
        {"conversation":2,"label":null,"parent":1},
        {"conversation":1,"label":null}
    ]);
    call(
        &catalog,
        "update",
        json!({
            "id":id, "title":"Chosen title", "main":2, "branches":branches, "archived":true
        }),
        &context,
    )
    .await?;
    let index = server
        .host
        .get(
            Arc::new(lookup("p2.session-index")?),
            wire(json!({})),
            &context,
        )
        .await?;
    let state = index.member("byId")?.value()?.to_json().to_serde()?;
    assert_eq!(state[&id]["branches"], branches);
    drop(index);
    drop(catalog);
    server.close().await?;

    let server = start(directory.path()).await?;
    let catalog = server.host.get(contract, wire(json!({})), &context).await?;
    call(
        &catalog,
        "activity",
        json!({
            "id":id, "timestamp":3, "firstUserText":"must not replace renamed title"
        }),
        &context,
    )
    .await?;
    let row = call(&catalog, "get", json!({"id":id}), &context).await?;
    assert_eq!(row["title"], "Chosen title");
    assert_eq!(row["main"], 2);
    assert_eq!(row["branches"], branches);
    assert!(row["archivedAt"].is_number());
    drop(catalog);
    server.close().await?;
    Ok(())
}

#[tokio::test]
async fn session_lock_is_worker_scoped_and_sidebar_preferences_reopen() -> TestResult {
    let directory = tempfile::tempdir()?;
    let server = start(directory.path()).await?;
    let context = Context::background();
    let hold = |server: &P2Server, contract: &str, principal| {
        server.host.hold_as(
            Arc::new(lookup(contract)?),
            wire(json!({})),
            Caller {
                principal: Some(principal),
                incarnation: "test".into(),
            },
            &context,
        )
    };
    let owner = json!({"kind":"owner","access":"local"});
    let lock =
        json!({"capabilities":["ports"],"instructions":true,"mcp":["test"],"projectMcp":false});
    let owner_locks = hold(&server, "p2.session-lock", owner.clone())?;
    call(
        &owner_locks,
        "set",
        json!({"session":"one","lock":lock}),
        &context,
    )
    .await?;
    let worker = hold(
        &server,
        "p2.session-lock",
        json!({"kind":"worker","session":"two","generation":1}),
    )?;
    worker.wait(&context).await?;
    assert!(
        worker
            .member("lock")?
            .value()?
            .to_json()
            .to_serde()?
            .is_null()
    );
    let sidebar = hold(&server, "p2.sidebar-settings", owner.clone())?;
    call(
        &sidebar,
        "setOrder",
        json!({"order":["archive","folders","archive"]}),
        &context,
    )
    .await?;
    call(
        &sidebar,
        "setWindow",
        json!({"key":"archive","window":"forever"}),
        &context,
    )
    .await?;
    let guest = hold(
        &server,
        "p2.sidebar-settings",
        json!({
            "kind":"guest","id":"g","home":"one","name":"Guest","sessions":{"one":"view"},"revision":1
        }),
    )?;
    assert!(
        matches!(guest.wait(&context).await, Err(Error::Chord(error)) if error.code == "forbidden")
    );
    drop((owner_locks, worker, sidebar, guest));
    server.close().await?;

    let server = start(directory.path()).await?;
    let worker = hold(
        &server,
        "p2.session-lock",
        json!({"kind":"worker","session":"one","generation":2}),
    )?;
    worker.wait(&context).await?;
    assert_eq!(worker.member("lock")?.value()?.to_json().to_serde()?, lock);
    let sidebar = hold(&server, "p2.sidebar-settings", owner.clone())?;
    sidebar.wait(&context).await?;
    assert_eq!(
        sidebar.member("order")?.value()?.to_json().to_serde()?,
        json!(["archive", "folders"])
    );
    assert_eq!(
        sidebar.member("windows")?.value()?.to_json().to_serde()?,
        json!({"archive":"forever"})
    );
    let owner_locks = hold(&server, "p2.session-lock", owner)?;
    call(
        &owner_locks,
        "set",
        json!({"session":"one","lock":null}),
        &context,
    )
    .await?;
    assert!(
        worker
            .member("lock")?
            .value()?
            .to_json()
            .to_serde()?
            .is_null()
    );
    drop((worker, sidebar, owner_locks));
    server.close().await?;
    Ok(())
}

#[tokio::test]
async fn lock_mirror_persists_and_clears_durable_session_lock_doc() -> TestResult {
    let dir = tempfile::tempdir()?;
    let storage = crate::runtime::session_store::open_session_storage(dir.path(), "s-lock")
        .await
        .map_err(|e| e.to_string())?;
    let context = Context::background();
    let harness = pi_rs_durable::Harness::open(
        storage.storage.clone(),
        pi_rs_durable::session::SessionOptions::default(),
        &context,
    )
    .await?;
    let lock = crate::runtime::session_lock::SessionLock {
        mcp: vec!["github".into()],
        project_mcp: true,
        instructions: true,
        capabilities: vec![crate::runtime::session_lock::LockCapability::Ports],
    };
    transfer::mirror_lock(&harness, Some(&lock), &context).await?;
    assert_eq!(
        crate::runtime::session_lock::session_lock(&harness.session, &context)
            .await
            .map_err(|e| e.to_string())?,
        Some(lock.clone())
    );
    transfer::mirror_lock(&harness, Some(&lock), &context).await?;
    transfer::mirror_lock(&harness, None, &context).await?;
    assert_eq!(
        crate::runtime::session_lock::session_lock(&harness.session, &context)
            .await
            .map_err(|e| e.to_string())?,
        None
    );
    harness.close(&context).await?;
    storage.close().await.map_err(|e| e.to_string())?;
    Ok(())
}
