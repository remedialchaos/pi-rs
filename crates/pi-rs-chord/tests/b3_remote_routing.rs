use pi_rs_chord::{
    Result,
    contracts::{Contract, HostKind, read_params},
    host::{HostOptions, create_host},
    json::JsonValue,
    link::{Body, Channel, ChannelHandler, ChannelOptions, Codec, Message, Part, memory_link_pair},
    remote::RemoteOptions,
    sync::SyncFuture,
};
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;

struct Receive(mpsc::UnboundedSender<Body>);
impl ChannelHandler for Receive {
    fn opened(&self) {}
    fn closed(&self) {}
    fn message(&self, message: Message) -> SyncFuture<()> {
        let _ = self.0.send(message.body);
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn validates_before_demand_stamps_child_identity_and_cancels_pending_routes() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let address_params = JsonValue::from_serde(serde_json::json!({}));
        let address = address_params.clone();
        let ty = Arc::new(Contract::remote(
            "routed",
            1,
            vec![],
            [("main".into(), Default::default())].into_iter().collect(),
            Arc::new(move |_| {
                HostKind::new("worker", read_params(&[], "worker")?)?.address(address.clone())
            }),
        )?);
        let fingerprint = u64::from_str_radix(ty.fingerprint(), 16)
            .expect("hex fingerprint")
            .to_be_bytes();
        let (requested, mut requests) = mpsc::unbounded_channel();
        let mut options = RemoteOptions::new(serde_json::json!("authenticated"));
        options.route_contracts.insert(ty.name().into(), ty);
        options.route = Some(Arc::new(move |request| {
            let _ = requested.send(request);
            Box::pin(std::future::pending())
        }));
        let host = create_host(HostOptions::default())?;
        let (a, b) = memory_link_pair();
        let server = host.accept(a, options)?;
        let (messages, mut received) = mpsc::unbounded_channel();
        let incarnation = [7; 16];
        let client = Channel::start(
            b,
            Arc::new(Receive(messages)),
            ChannelOptions {
                codec: Codec::Binary,
                incarnation,
                calls: None,
                report: Arc::new(|_| {}),
            },
        )?;
        client.fence().await?;
        let hold = |hold, version, fingerprint| {
            Message::new(Body::Hold {
                hold,
                type_name: "routed".into(),
                version,
                fingerprint,
                params: Part::Text("{}".into()),
                address_kind: "worker".into(),
                address_params: Box::new(address_params.clone()),
                principal: Some(Box::new(JsonValue::from_serde(
                    serde_json::json!({"kind":"forged"}),
                ))),
                incarnation: Some([8; 16]),
                chain: None,
                limit: None,
            })
        };
        client.send(hold(1, 2, fingerprint))?;
        assert!(matches!(
            received.recv().await,
            Some(Body::Failed { hold: 1, .. })
        ));
        client.send(hold(2, 1, [0; 8]))?;
        assert!(matches!(
            received.recv().await,
            Some(Body::Failed { hold: 2, .. })
        ));
        assert!(requests.try_recv().is_err());
        client.send(hold(3, 1, fingerprint))?;
        assert!(matches!(
            received.recv().await,
            Some(Body::Loading { hold: 3 })
        ));
        let request = requests.recv().await.expect("route was consulted");
        assert_eq!(
            request.caller.principal,
            Some(serde_json::json!("authenticated"))
        );
        assert_eq!(
            request.caller.incarnation,
            uuid::Uuid::from_bytes(incarnation).to_string()
        );
        assert_eq!(request.address_kind, "worker");
        assert_eq!(request.address_params, address_params);
        client.send(Message::new(Body::Call {
            call: 1,
            hold: 3,
            method: 0,
            args: None,
        }))?;
        assert!(matches!(
            received.recv().await,
            Some(Body::Error { call: 1, .. })
        ));
        client.send(Message::new(Body::Release { hold: 3 }))?;
        client.fence().await?;
        assert!(request.context.is_aborted());
        client.send(hold(3, 1, fingerprint))?;
        let next = requests.recv().await.expect("hold ID is reusable");
        server.close();
        assert!(next.context.is_aborted());
        Ok(())
    })
    .await
    .expect("route protocol stalled")
}
