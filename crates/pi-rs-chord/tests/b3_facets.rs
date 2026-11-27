use pi_rs_chord::{context::Context, contracts::schema as s, json::JsonValue, node::app::*};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

fn options(dir: &std::path::Path) -> AppOptions {
    AppOptions::new(
        "example",
        dir.into(),
        json!({"kind":"owner"}),
        json!({"kind":"server"}),
    )
}

async fn call(held: &pi_rs_chord::host::Held, method: &str, args: Value) -> Value {
    held.call(method, JsonValue::from_serde(args), &Context::background())
        .await
        .unwrap()
        .to_serde()
        .unwrap()
}

#[tokio::test]
async fn files_facet_pages_and_keeps_folder_and_space_state_live() {
    let dir = tempfile::tempdir().unwrap();
    let server = start_server(options(dir.path())).await.unwrap();
    let ctx = Context::background();
    let files = server
        .host
        .get(
            Arc::new(files_contract().unwrap()),
            JsonValue::from_serde(json!({})),
            &ctx,
        )
        .await
        .unwrap();
    let folder = server
        .host
        .get(
            Arc::new(files_folder_contract().unwrap()),
            JsonValue::from_serde(json!({"space":"shared","folder":""})),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(
        folder.read("listing").unwrap().to_serde().unwrap(),
        json!({"entries":[],"more":false})
    );
    let first = call(&files, "put", json!({"space":"shared","path":"a","mime":"text/plain","source":"test","ref":"producer","key":"once","bytes":[0,128,255]})).await;
    assert_eq!(
        call(
            &files,
            "read",
            json!({"id":first["id"],"offset":1,"length":8})
        )
        .await,
        json!([128, 255])
    );
    assert_eq!(call(&files, "begin", json!({"space":"shared","path":"elsewhere","mime":"text/plain","source":"test","key":"once"})).await, json!({"kind":"exists","file":first}));
    assert_eq!(
        call(&files, "mkdir", json!({"space":"shared","path":"docs"})).await["path"],
        "docs"
    );
    let first_page = call(
        &files,
        "list",
        json!({"space":"shared","folder":"","limit":1}),
    )
    .await;
    assert_eq!(first_page["items"][0]["folder"]["path"], "docs");
    let second = call(
        &files,
        "list",
        json!({"space":"shared","folder":"","limit":1,"after":first_page["next"]}),
    )
    .await;
    assert_eq!(second, json!({"items":[{"kind":"file","file":first}]}));
    assert_eq!(
        folder.read("listing").unwrap().to_serde().unwrap()["entries"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        call(&files, "byRef", json!({"space":"shared","ref":"producer"})).await["items"],
        json!([first])
    );
    assert_eq!(
        call(&files, "byKey", json!({"space":"shared","key":"once"})).await,
        first
    );
    let copy = call(
        &files,
        "copy",
        json!({"id":first["id"],"space":"other","path":"saved"}),
    )
    .await;
    assert_eq!(copy["ref"], "producer");
    assert!(copy.get("key").is_none());
    let second_version = call(
        &files,
        "put",
        json!({"space":"shared","path":"a","mime":"text/plain","source":"test","bytes":[5]}),
    )
    .await;
    let page = call(
        &files,
        "versions",
        json!({"space":"shared","path":"a","limit":1}),
    )
    .await;
    assert_eq!(page["items"], json!([second_version]));
    call(&files, "remove", second_version["id"].clone()).await;
    assert_eq!(
        call(
            &files,
            "versions",
            json!({"space":"shared","path":"a","after":page["next"]})
        )
        .await["items"],
        json!([first])
    );
    call(
        &files,
        "move",
        json!({"space":"shared","from":"a","to":"docs/a"}),
    )
    .await;
    assert_eq!(
        call(
            &files,
            "get",
            json!({"space":"shared","path":"docs/a","version":1})
        )
        .await["id"],
        first["id"]
    );
    call(
        &files,
        "removePath",
        json!({"space":"shared","path":"docs"}),
    )
    .await;
    assert_eq!(call(&files, "file", first["id"].clone()).await, Value::Null);
    assert_eq!(
        call(
            &files,
            "read",
            json!({"id":copy["id"],"offset":0,"length":3})
        )
        .await,
        json!([0, 128, 255])
    );
    let listing = folder.read("listing").unwrap().to_serde().unwrap();
    assert_eq!(listing["entries"], json!([]));
    assert_eq!(
        files
            .keyed("summary", &JsonValue::String("other".into()))
            .unwrap()
            .value()
            .unwrap()
            .to_json()
            .to_serde()
            .unwrap()["bytes"],
        3
    );
    call(&files, "removeSpace", json!("other")).await;
    assert_eq!(
        folder.read("listing").unwrap().to_serde().unwrap()["entries"],
        json!([])
    );
    assert_eq!(
        files
            .keyed("summary", &JsonValue::String("other".into()))
            .unwrap()
            .value()
            .unwrap()
            .to_json()
            .to_serde()
            .unwrap(),
        json!({"files":0,"versions":0,"bytes":0,"newest":[]})
    );
    server.close().await.unwrap();
}

#[tokio::test]
async fn files_facet_expires_idle_uploads_but_preserves_recent_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let server = start_server(options(dir.path())).await.unwrap();
    let ctx = Context::background();
    let files = server
        .host
        .get(
            Arc::new(files_contract().unwrap()),
            JsonValue::from_serde(json!({})),
            &ctx,
        )
        .await
        .unwrap();
    let uploads = server.files.directory().join(".uploads");
    let args = |path| json!({"space":"uploads","path":path,"mime":"text/plain","source":"test","key":path});

    tokio::time::pause();
    let idle = call(&files, "begin", args("idle")).await["upload"].clone();
    let active = call(&files, "begin", args("active")).await["upload"].clone();
    assert_eq!(
        call(&files, "begin", args("active")).await["upload"],
        active
    );
    call(
        &files,
        "append",
        json!({"upload": idle, "offset": 0, "bytes":[1,2]}),
    )
    .await;
    tokio::time::advance(Duration::from_secs(3590)).await;
    call(
        &files,
        "append",
        json!({"upload": active, "offset": 0, "bytes":[3]}),
    )
    .await;
    tokio::time::advance(Duration::from_secs(70)).await;
    tokio::task::yield_now().await;
    assert!(!uploads.join(idle.as_str().unwrap()).exists());
    assert!(
        files
            .call("uploaded", JsonValue::from_serde(idle), &ctx)
            .await
            .is_err()
    );
    assert_eq!(call(&files, "uploaded", active.clone()).await, 1);
    tokio::time::resume();
    let file = call(&files, "finish", active).await;
    assert_eq!(file["size"], 1);
    let unfinished = call(&files, "begin", args("unfinished")).await["upload"].clone();
    assert!(uploads.join(unfinished.as_str().unwrap()).exists());
    files.release();
    server.close().await.unwrap();
    assert!(!uploads.join(unfinished.as_str().unwrap()).exists());
}

#[tokio::test]
async fn users_facet_validates_profiles_and_persists_accounts_through_restart() {
    let dir = tempfile::tempdir().unwrap();
    let start = || {
        let profile = s::obj([("name", s::required(s::str()))]);
        let (contract, _) = define_users(profile.clone()).unwrap();
        let mut options = options(dir.path());
        options.users_profile = profile;
        async move { (start_server(options).await.unwrap(), contract) }
    };
    let ctx = Context::background();
    let (server, contract) = start().await;
    let held = server
        .host
        .get(contract, JsonValue::from_serde(json!({})), &ctx)
        .await
        .unwrap();
    assert!(
        held.call(
            "update",
            JsonValue::from_serde(json!({"id":"x","profile":{"name":42}})),
            &ctx
        )
        .await
        .is_err()
    );
    assert!(
        held.call(
            "update",
            JsonValue::from_serde(json!({"id":"__proto__","profile":{"name":"no"}})),
            &ctx
        )
        .await
        .is_err()
    );
    call(
        &held,
        "update",
        json!({"id":"x","profile":{"name":"first"}}),
    )
    .await;
    assert_eq!(
        held.read("list").unwrap().to_serde().unwrap(),
        json!([{"id":"x","profile":{"name":"first"}}])
    );
    server.close().await.unwrap();
    let (server, contract) = start().await;
    let held = server
        .host
        .get(contract, JsonValue::from_serde(json!({})), &ctx)
        .await
        .unwrap();
    assert_eq!(
        call(&held, "get", json!("x")).await,
        json!({"name":"first"})
    );
    call(&held, "remove", json!("x")).await;
    assert_eq!(held.read("list").unwrap().to_serde().unwrap(), json!([]));
    assert_eq!(call(&held, "get", json!("x")).await, Value::Null);
    server.close().await.unwrap();
}
