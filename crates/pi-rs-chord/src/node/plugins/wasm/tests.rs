use super::*;
use crate::{
    host::{HostOptions, PluginFacet, create_host},
    registries::define_registry,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const FACET: &[u8] = include_bytes!("../../../../tests/fixtures/b3/facet.wasm");
const REJECT: &[u8] = include_bytes!("../../../../tests/fixtures/b3/reject.wasm");

#[tokio::test]
async fn guest_setup_failure_delivers_diagnostic_to_host_logger() -> Result<()> {
    let messages = Arc::new(Mutex::new(Vec::new()));
    let received = messages.clone();
    let host = WasmPluginHost::with_logger(move |msg| {
        received
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(msg.to_owned())
    })?;
    assert!(host.prepare(REJECT).await.is_err());
    assert_eq!(
        *messages.lock().unwrap_or_else(|e| e.into_inner()),
        ["rejecting replacement"]
    );
    Ok(())
}

#[tokio::test]
async fn guest_jobs_publish_documents_and_parameterized_members_route_events_and_keys() -> Result<()>
{
    // Verify the import independently of document and job activation.
    use super::bindings::chord::plugin::host::Host as _;
    let wasm = WasmPluginHost::new()?;
    let channel = EventChannel::new();
    let event = Arc::new(Mutex::new(None));
    let received = event.clone();
    let _subscription = channel
        .subscribe(move |value| *received.lock().unwrap_or_else(|e| e.into_inner()) = Some(value));
    let sources = Sources::default();
    sources
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(("test".into(), "tick".into()), Publisher::Event(channel));
    let mut store = wasm.store(sources)?;
    store.data_mut().active_object = Some("test".into());
    store
        .data_mut()
        .publish("test".into(), "tick".into(), "3".into())
        .await
        .map_err(failure)?;
    assert_eq!(
        *event.lock().unwrap_or_else(|e| e.into_inner()),
        Some(JsonValue::Number(3.))
    );
    drop(store);
    let ctx = Context::background();
    let host = create_host(HostOptions {
        principal: Some(serde_json::json!("operator")),
        address: Some(JsonValue::parse(
            r#"{"kind":"stage","params":{"region":"eu"}}"#,
        )?),
        ..Default::default()
    })?;
    let facet = wasm.prepare(FACET).await?;
    let report = host
        .load(vec![PluginFacet::new("guest", facet)], &ctx)
        .await?;
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(
        host.registry(define_registry("guest-registry", 1)?)
            .value(1)?,
        JsonValue::parse("[42]")?
    );
    let contract = host
        .served_contract("guest-counter")
        .ok_or_else(|| failure("missing guest-counter"))?;
    let held = host
        .get(
            contract.clone(),
            JsonValue::parse(r#"{"room":"alpha"}"#)?,
            &ctx,
        )
        .await?;
    let other = host
        .get(contract, JsonValue::parse(r#"{"room":"other"}"#)?, &ctx)
        .await?;
    let observed = Arc::new(Mutex::new(Vec::new()));
    let listener = observed.clone();
    let _sub = held.subscribe_event("tick", move |v| {
        listener.lock().unwrap_or_else(|e| e.into_inner()).push(v)
    })?;
    assert_eq!(
        held.call("add", JsonValue::Number(3.), &ctx).await?,
        JsonValue::Number(3.)
    );
    assert_eq!(
        *observed.lock().unwrap_or_else(|e| e.into_inner()),
        [JsonValue::Number(3.)]
    );
    assert_eq!(
        held.keyed("rows", &JsonValue::parse(r#""a""#)?)?
            .value()?
            .to_json(),
        JsonValue::Number(3.)
    );
    assert_eq!(
        held.keyed("rows", &JsonValue::parse(r#""b""#)?)?
            .value()?
            .to_json(),
        JsonValue::Number(0.)
    );
    assert_eq!(
        held.call("update", JsonValue::Number(4.), &ctx).await?,
        JsonValue::Number(3.)
    );
    assert!(
        held.call("update", JsonValue::Number(-1.), &ctx)
            .await
            .is_err()
    );
    assert_eq!(held.read("total")?, JsonValue::parse(r#"{"total":7}"#)?);
    let denied = held
        .call("foreign", JsonValue::Null, &ctx)
        .await
        .err()
        .ok_or_else(|| failure("read another object"))?;
    assert!(denied.to_string().contains("current object"), "{denied}");
    assert_eq!(other.read("total")?, JsonValue::parse(r#"{"total":0}"#)?);
    let contract = host
        .served_contract("guest-remote")
        .ok_or_else(|| failure("missing guest-remote"))?;
    let params = JsonValue::parse(r#"{"region":"eu","room":"alpha"}"#)?;
    let address = contract.host_address(&params)?;
    assert_eq!(address.kind, "stage");
    assert_eq!(address.params, JsonValue::parse(r#"{"region":"eu"}"#)?);
    let remote = host.get(contract, params, &ctx).await?;
    assert_eq!(remote.variant()?, "main");
    assert_eq!(
        remote.call("who", JsonValue::Null, &ctx).await?,
        JsonValue::parse(r#""operator""#)?
    );
    remote.release();
    other.release();
    held.release();
    let http = crate::node::http::Http::new(host, Default::default());
    let server = http
        .listen(([127, 0, 0, 1], 0).into())
        .await
        .map_err(failure)?;
    let mut connection = tokio::net::TcpStream::connect(server.address).await?;
    connection.write_all(format!("POST /guest/abc HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: 1\r\nConnection: close\r\n\r\n8",server.address).as_bytes()).await?;
    let mut response = String::new();
    connection.read_to_string(&mut response).await?;
    server.close().await.map_err(failure)?;
    let (head, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| failure("missing HTTP response body"))?;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(body)?,
        serde_json::json!({"params":{"id":"abc"},"body":8,"principal":null})
    );
    Ok(())
}
