use std::{error::Error, sync::Arc, time::Duration};

use futures_util::StreamExt;
use pi_rs_chord::{
    context::Context,
    host::{HostOptions, create_host},
    node::http::{Http, HttpContribution, HttpOptions},
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Error as SocketError, Message, client::IntoClientRequest},
};

#[tokio::test]
async fn authenticated_stale_pages_receive_reload_frame_before_chord_hello()
-> Result<(), Box<dyn Error>> {
    let host = create_host(HostOptions::default())?;
    let mut auth = HttpContribution::new("auth");
    auth.authenticators.push(Arc::new(|request| {
        Box::pin(async move {
            Ok((request.header("authorization") == Some("Bearer fixture"))
                .then(|| serde_json::json!({"kind":"owner","access":"local"})))
        })
    }));
    let http = Http::new(
        host.clone(),
        HttpOptions {
            links_path: Some("/chord".into()),
            admit_link: |req| super::admit_link(req, Some("current")),
            ..Default::default()
        },
    );
    http.contribute(auth)?;
    let server = http.listen(([127, 0, 0, 1], 0).into()).await?;
    for (query, stale) in [
        ("", true),
        ("client=unknown", true),
        ("web=old&client=tui", true),
        ("web=old&web=current", true),
        ("web=c%75rrent", false),
        ("client=tui", false),
        ("client=cloud", false),
        ("client=scr%69pt", false),
    ] {
        let mut request = format!("ws://{}/chord?{query}", server.address).into_client_request()?;
        request
            .headers_mut()
            .insert("authorization", "Bearer fixture".parse()?);
        let (mut socket, _) = connect_async(request).await?;
        let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await?
            .ok_or("missing server frame")??;
        if stale {
            let Message::Close(Some(frame)) = message else {
                return Err(format!("expected a reload close frame for {query}").into());
            };
            assert_eq!(u16::from(frame.code), 4001);
            assert_eq!(frame.reason.as_str(), "p2: stale page, reload");
        } else {
            assert!(
                matches!(message, Message::Binary(_)),
                "missing Chord hello for {query}"
            );
        }
    }
    let anonymous = connect_async(format!("ws://{}/chord?web=old", server.address)).await;
    assert!(
        matches!(anonymous, Err(SocketError::Http(response)) if response.status() == 403 && response.body().as_deref().unwrap_or_default().is_empty())
    );
    assert!(super::stale_page("web=old", None).is_none());
    assert!(super::stale_page("", None).is_some());
    server.close().await?;
    host.close(&Context::background()).await?;
    Ok(())
}
