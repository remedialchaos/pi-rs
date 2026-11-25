use indexmap::IndexMap;
use pi_rs_chord::{
    Result,
    context::Context,
    contracts::{Contract, Member, schema::*},
    host::*,
    json::JsonValue,
    node::http::*,
    remote::{RemoteOptions, RemotePeer},
    state::{Batch, Source},
};
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn live_binary_websocket_rpc_and_readable_updates_use_host_principal() -> Result<()> {
    let contract = Arc::new(Contract::remote(
        "network.counter",
        1,
        vec![],
        IndexMap::from([(
            "main".into(),
            IndexMap::from([
                ("value".into(), Member::State(int())),
                ("add".into(), method(Some(int()), Some(int()))),
                ("who".into(), method(None, Some(str()))),
            ]),
        )]),
        Arc::new(|_| {
            pi_rs_chord::contracts::HostKind::new(
                "server",
                pi_rs_chord::contracts::read_params(&[], "server")?,
            )?
            .address(JsonValue::Object(IndexMap::new()))
        }),
    )?);
    let host = create_host(HostOptions {
        principal: Some(serde_json::json!("server")),
        address: Some(JsonValue::from_serde(
            serde_json::json!({"kind":"server","params":{}}),
        )),
        ..Default::default()
    })?;
    let value = Source::new(host.batch(), JsonValue::Number(1.0))?;
    let writable = value.clone();
    let ty = contract.clone();
    host.load(
        vec![PluginFacet::new(
            "counter",
            define_facet(move |setup| {
                let value = writable.clone();
                setup.serve(
                    ty.clone(),
                    move |_, _, _| {
                        let mut members = Members::variant("main");
                        members.state("value", value.clone())?;
                        let value = value.clone();
                        members.method("add", move |args, _| {
                            let value = value.clone();
                            Box::pin(async move {
                                let (JsonValue::Number(n), JsonValue::Number(amount)) =
                                    (value.value()?.to_json(), args)
                                else {
                                    return Err(pi_rs_chord::Error::Invalid(
                                        "numbers required".into(),
                                    ));
                                };
                                let next = JsonValue::Number(n + amount);
                                value.set(next.clone(), &Context::background())?;
                                Ok(next)
                            })
                        })?;
                        members.method("who", |_, ctx| {
                            Box::pin(async move {
                                Ok(JsonValue::from_serde(
                                    ctx.principal.clone().unwrap_or_default(),
                                ))
                            })
                        })?;
                        Ok(members)
                    },
                    ServeOptions::default(),
                )
            }),
        )],
        &Context::background(),
    )
    .await?;
    let http = Http::new(host.clone(), HttpOptions::default());
    let mut auth = HttpContribution::new("auth");
    auth.authenticators.push(Arc::new(|r| {
        Box::pin(async move {
            Ok((r.header("authorization") == Some("Bearer test"))
                .then(|| serde_json::json!("reader")))
        })
    }));
    http.contribute(auth)
        .map_err(|e| pi_rs_chord::Error::Invalid(e.to_string()))?;
    let server = http
        .listen("127.0.0.1:0".parse().unwrap())
        .await
        .map_err(|e| pi_rs_chord::Error::Invalid(e.to_string()))?;
    let url = format!("ws://{}/_chord/link", server.address);
    assert!(
        connect_websocket(&url, http::HeaderMap::new(), 1024 * 1024, 8 * 1024 * 1024)
            .await
            .is_err()
    );
    let headers = http::HeaderMap::from_iter([(
        http::header::AUTHORIZATION,
        http::HeaderValue::from_static("Bearer test"),
    )]);
    let link = connect_websocket(&url, headers, 1024 * 1024, 8 * 1024 * 1024).await?;
    let client = RemotePeer::connect(
        link,
        RemoteOptions::new(serde_json::json!("server")),
        Batch::new(|_| {}),
    )?;
    let held = client.hold(
        contract,
        JsonValue::Object(IndexMap::new()),
        &Context::background(),
    )?;
    tokio::time::timeout(Duration::from_secs(5), held.wait(&Context::background()))
        .await
        .unwrap()?;
    assert_eq!(
        held.member("value")?.value()?.to_json(),
        JsonValue::Number(1.0)
    );
    assert_eq!(
        held.call("who", JsonValue::Null, &Context::background())
            .await?,
        JsonValue::String("reader".into())
    );
    assert_eq!(
        held.call("add", JsonValue::Number(4.0), &Context::background())
            .await?,
        JsonValue::Number(5.0)
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while held.member("value").unwrap().value().unwrap().to_json() != JsonValue::Number(5.0) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    value.set(JsonValue::Number(42.0), &Context::background())?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while held.member("value").unwrap().value().unwrap().to_json() != JsonValue::Number(42.0) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    held.release();
    client.close();
    server
        .close()
        .await
        .map_err(|e| pi_rs_chord::Error::Invalid(e.to_string()))?;
    host.close(&Context::background()).await?;
    Ok(())
}
