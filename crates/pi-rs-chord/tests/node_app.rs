use pi_rs_chord::{context::Context, json::JsonValue, node::app::*};
use serde_json::{Value, json};
use std::{net::SocketAddr, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn post(address: SocketAddr, path: &str, headers: &str, body: Value) -> (u16, String, Value) {
    let body = serde_json::to_string(&body).unwrap();
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream.write_all(format!("POST {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{headers}\r\n{body}",body.len()).as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    let text = String::from_utf8(bytes).unwrap();
    let (headers, body) = text.split_once("\r\n\r\n").unwrap();
    let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
    (
        status,
        headers.into(),
        serde_json::from_str(body).unwrap_or(Value::Null),
    )
}
fn cookie(headers: &str) -> String {
    headers
        .lines()
        .find_map(|line| line.strip_prefix("set-cookie: "))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .into()
}
#[tokio::test]
async fn server_boot_owner_pairing_invites_ephemeral_and_persistent_restart() {
    let temp = tempfile::tempdir().unwrap();
    let owner = json!({"kind":"owner"});
    let options = || {
        AppOptions::new(
            "example",
            temp.path().join("data"),
            owner.clone(),
            json!({"kind":"server"}),
        )
    };
    let server = start_server(options()).await.unwrap();
    assert!(start_server(options()).await.is_err());
    let held = tokio::time::timeout(
        Duration::from_secs(10),
        server.host.get(
            std::sync::Arc::new(app_info_contract().unwrap()),
            JsonValue::from_serde(json!({})),
            &Context::background(),
        ),
    )
    .await
    .expect("AppInfo must become ready")
    .unwrap();
    assert_eq!(
        held.member("name")
            .unwrap()
            .value()
            .unwrap()
            .to_json()
            .to_serde()
            .unwrap(),
        json!("example")
    );
    drop(held);
    let token = server.owner_link.split_once('#').unwrap().1;
    let (status, headers, _) =
        post(server.address, "/_chord/owner", "", json!({"token":token})).await;
    assert_eq!(status, 200);
    let owner_cookie = cookie(&headers);
    let auth = format!("Cookie: {owner_cookie}\r\n");
    assert_eq!(
        post(server.address, "/_chord/owner", "", json!({"token":token}))
            .await
            .0,
        403
    );
    let (_, _, pair) = post(
        server.address,
        "/_chord/pair",
        "",
        json!({"label":"offline device"}),
    )
    .await;
    assert_eq!(
        post(
            server.address,
            "/_chord/pair/poll",
            "",
            json!({"poll":pair["poll"]})
        )
        .await
        .2,
        json!({"status":"waiting"})
    );
    assert_eq!(
        post(
            server.address,
            &format!("/_chord/pair/{}", pair["code"].as_str().unwrap()),
            &auth,
            json!({})
        )
        .await
        .0,
        200
    );
    let (_, _, approved) = post(
        server.address,
        "/_chord/pair/poll",
        "",
        json!({"poll":pair["poll"]}),
    )
    .await;
    assert_eq!(approved["status"], "approved");
    assert_eq!(
        server
            .sign_ins
            .verify(approved["token"].as_str().unwrap())
            .await
            .unwrap(),
        Some(owner.clone())
    );
    assert_eq!(
        post(
            server.address,
            "/_chord/pair/poll",
            "",
            json!({"poll":pair["poll"]})
        )
        .await
        .0,
        404
    );
    let principal = json!({"kind":"user","id":"invited"});
    let (_, _, invite) = post(
        server.address,
        "/_chord/invites",
        &auth,
        json!({"principal":principal,"label":"Invite"}),
    )
    .await;
    let invite = invite["path"].as_str().unwrap().split_once('#').unwrap().1;
    let (status, _, _) = post(
        server.address,
        "/_chord/invite",
        "",
        json!({"token":invite}),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        post(
            server.address,
            "/_chord/invite",
            "",
            json!({"token":invite})
        )
        .await
        .0,
        403
    );
    let (status, headers, user) = post(
        server.address,
        "/_chord/login",
        "",
        json!({"name":" Guest "}),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(user["name"], "Guest");
    assert!(!headers.contains("Max-Age="));
    assert_eq!(
        post(
            server.address,
            "/_chord/invites",
            "",
            json!({"principal":{},"label":"no"})
        )
        .await
        .0,
        401
    );
    let owner_token = std::fs::read_to_string(&server.layout.owner_token).unwrap();
    assert_eq!(
        server.sign_ins.verify(owner_token.trim()).await.unwrap(),
        Some(owner.clone())
    );
    server.close().await.unwrap();
    let server = start_server(options()).await.unwrap();
    assert!(
        server
            .sign_ins
            .verify(owner_token.trim())
            .await
            .unwrap()
            .is_some()
    );
    server.close().await.unwrap();
}
#[tokio::test]
async fn files_versions_uploads_live_folder_traversal_and_archive_roundtrip() {
    let temp = tempfile::tempdir().unwrap();
    let server = start_server(AppOptions::new(
        "example",
        temp.path().join("data"),
        json!({"kind":"owner"}),
        json!({"kind":"server"}),
    ))
    .await
    .unwrap();
    let files = &server.files;
    let folder = files.folder("shared", "docs").unwrap();
    let first = files
        .put(FileWrite::new("shared", "docs/readme.txt"), b"first")
        .await
        .unwrap();
    assert_eq!(
        folder.source.value().unwrap().to_json().to_serde().unwrap()["entries"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let second = files
        .put(FileWrite::new("shared", "docs/readme.txt"), b"second")
        .await
        .unwrap();
    assert_eq!(second.version, 2);
    assert_eq!(files.read(&first.id, 1, 3).unwrap(), b"irs");
    let mut write = FileWrite::new("shared", "upload.bin");
    write.key = Some("request-1".into());
    let upload = files.begin(write.clone()).unwrap();
    assert_eq!(files.begin(write.clone()).unwrap(), upload);
    files.append(&upload, 0, b"abc").await.unwrap();
    assert!(files.append(&upload, 0, b"bad").await.is_err());
    files.append(&upload, 3, b"def").await.unwrap();
    assert_eq!(files.uploaded(&upload).unwrap(), 6);
    let uploaded = files.finish(&upload).await.unwrap();
    assert_eq!(files.read(&uploaded.id, 0, 20).unwrap(), b"abcdef");
    assert_eq!(files.put(write, b"not stored").await.unwrap(), uploaded);
    let copied = files
        .copy(&first.id, FileWrite::new("shared", "copy.txt"))
        .await
        .unwrap();
    assert_eq!(files.read(&copied.id, 0, 10).unwrap(), b"first");
    files
        .move_path("shared", "copy.txt", "renamed.txt")
        .await
        .unwrap();
    assert!(files.get("shared", "copy.txt", None).unwrap().is_none());
    for path in ["../escape", "/absolute", "a/./b", "a//b"] {
        assert!(
            files
                .put(FileWrite::new("shared", path), b"bad")
                .await
                .is_err()
        );
    }
    files
        .remove("shared", "docs/readme.txt", Some(2))
        .await
        .unwrap();
    assert_eq!(
        files
            .get("shared", "docs/readme.txt", None)
            .unwrap()
            .unwrap()
            .version,
        1
    );
    let third = files
        .put(FileWrite::new("shared", "docs/readme.txt"), b"third")
        .await
        .unwrap();
    assert_eq!(third.version, 3);
    let archive = server.export_server().await.unwrap();
    assert!(
        read_tar(&archive, ArchiveLimits::default())
            .unwrap()
            .iter()
            .any(|e| e.path == "manifest.json")
    );
    let address = ArchiveAddress {
        kind: "server".into(),
        key: "server".into(),
    };
    let (_, imported) = unpack_staging(
        &archive,
        "example",
        &address,
        &temp.path().join("import"),
        ArchiveLimits::default(),
    )
    .await
    .unwrap();
    let restored = Files::open(
        imported.clone(),
        temp.path().join("import/files"),
        FilesCaps::default(),
    )
    .await
    .unwrap();
    assert_eq!(restored.read(&uploaded.id, 0, 20).unwrap(), b"abcdef");
    assert_eq!(
        restored
            .get("shared", "docs/readme.txt", None)
            .unwrap()
            .unwrap()
            .version,
        3
    );
    imported.database().close().await.unwrap();
    server.close().await.unwrap();
}
#[test]
fn tar_rejects_traversal_corruption_truncation_and_resource_exhaustion() {
    let entries = vec![TarEntry {
        path: "files/blobs/data".into(),
        bytes: b"content".to_vec(),
        directory: false,
    }];
    let bytes = write_tar(&entries).unwrap();
    assert_eq!(read_tar(&bytes, ArchiveLimits::default()).unwrap(), entries);
    let mut bad = bytes.clone();
    bad[0] ^= 1;
    assert!(read_tar(&bad, ArchiveLimits::default()).is_err());
    assert!(read_tar(&bytes[..600], ArchiveLimits::default()).is_err());
    assert!(
        read_tar(
            &bytes,
            ArchiveLimits {
                bytes: 1,
                entries: 10
            }
        )
        .is_err()
    );
    assert!(
        write_tar(&[TarEntry {
            path: "../secret".into(),
            bytes: vec![],
            directory: false
        }])
        .is_err()
    );
}

#[tokio::test]
async fn worker_app_boot_reports_ready_and_serves_worker_info_then_closes_store() {
    use pi_rs_chord::node::workers::{
        Control, Frame, ProcessLock, WorkerSession, read_frame, write_frame,
    };
    let temp = tempfile::tempdir().unwrap();
    let lock = temp.path().join("worker.lock");
    let (parent, child) = tokio::io::duplex(8192);
    let (mut reader, mut writer) = tokio::io::split(parent);
    let (input, output) = tokio::io::split(child);
    let address = json!({"kind":"worker","params":{"id":"one"},"key":"worker|one"});
    write_frame(
        &mut writer,
        &Frame::Control(Control::Start {
            address: address.clone(),
            start: json!({"hello":"worker"}),
            scripts: vec![],
            lock: lock.clone(),
        }),
    )
    .await
    .unwrap();
    let session = WorkerSession::start(input, output).await.unwrap();
    assert!(matches!(
        read_frame(&mut reader).await.unwrap(),
        Frame::Control(Control::Started)
    ));
    assert!(ProcessLock::try_acquire(&lock).unwrap().is_none());
    let app = start_worker(
        session,
        WorkerAppOptions {
            directory: temp.path().join("data"),
            principal: json!({"kind":"worker"}),
            server_principal: json!({"kind":"server"}),
            core: vec![],
            files: FilesCaps::default(),
        },
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match read_frame(&mut reader).await.unwrap() {
                Frame::Control(Control::Ready { .. }) => break,
                Frame::Binary(_) => {}
                other => panic!("unexpected worker startup frame: {other:?}"),
            }
        }
    })
    .await
    .unwrap();
    let info = tokio::time::timeout(
        Duration::from_secs(10),
        app.host.get(
            std::sync::Arc::new(worker_info_contract().unwrap()),
            JsonValue::from_serde(json!({})),
            &Context::background(),
        ),
    )
    .await
    .expect("WorkerInfo must become ready")
    .unwrap();
    assert_eq!(
        info.member("address")
            .unwrap()
            .value()
            .unwrap()
            .to_json()
            .to_serde()
            .unwrap(),
        address
    );
    drop(info);
    write_frame(&mut writer, &Frame::Control(Control::Shutdown))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), app.run())
        .await
        .unwrap()
        .unwrap();
    assert!(ProcessLock::try_acquire(&lock).unwrap().is_some());
}
