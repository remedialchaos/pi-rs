use crate::{
    Error, Result,
    link::{Link, WebSocketLink, WebSocketPort},
};
use futures_util::{SinkExt, StreamExt};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::sync::{mpsc, watch};

enum Out {
    Bytes(Vec<u8>),
}
struct Port {
    sender: mpsc::UnboundedSender<Out>,
    closed: watch::Sender<Option<u16>>,
    ended: AtomicBool,
    buffered: AtomicUsize,
    maximum: usize,
}
impl WebSocketPort for Port {
    fn ready_state(&self) -> u8 {
        if self.ended.load(Ordering::Acquire) {
            3
        } else {
            1
        }
    }
    fn buffered_amount(&self) -> usize {
        self.buffered.load(Ordering::Acquire)
    }
    fn send_binary(&self, bytes: Vec<u8>) -> Result<()> {
        let len = bytes.len().max(1);
        let mut current = self.buffered.load(Ordering::Acquire);
        loop {
            let Some(next) = current.checked_add(len).filter(|n| *n <= self.maximum) else {
                return Err(Error::Invalid("websocket output exceeds bound".into()));
            };
            match self.buffered.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
        if self.sender.send(Out::Bytes(bytes)).is_err() {
            self.buffered.fetch_sub(len, Ordering::AcqRel);
            return Err(Error::Closed);
        }
        Ok(())
    }
    fn close(&self, code: u16) {
        self.ended.store(true, Ordering::Release);
        self.closed.send_replace(Some(code));
    }
    fn detach_messages(&self) {
        self.ended.store(true, Ordering::Release);
    }
}
fn port(
    max_payload: usize,
    max_buffered: usize,
) -> (Arc<WebSocketLink>, Arc<Port>, mpsc::UnboundedReceiver<Out>) {
    let (sender, receiver) = mpsc::unbounded_channel();
    let (closed, _) = watch::channel(None);
    let port = Arc::new(Port {
        sender,
        closed,
        ended: AtomicBool::new(false),
        buffered: AtomicUsize::new(0),
        maximum: max_buffered,
    });
    let link = WebSocketLink::new(port.clone(), max_payload, max_buffered);
    (link, port, receiver)
}

pub(super) fn server_link(
    socket: axum::extract::ws::WebSocket,
    max_payload: usize,
    max_buffered: usize,
) -> (
    Arc<dyn Link>,
    impl std::future::Future<Output = ()> + Send + 'static,
) {
    let (link, port, mut outgoing) = port(max_payload, max_buffered);
    let output: Arc<dyn Link> = link.clone();
    let run = async move {
        use axum::extract::ws::{CloseFrame, Message};
        let mut closed = port.closed.subscribe();
        if closed.borrow().is_some() {
            closed.mark_changed();
        }
        let (mut sender, mut receiver) = socket.split();
        loop {
            tokio::select! { biased;
                _ = closed.changed() => {
                    let code = *closed.borrow_and_update();
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(1),sender.send(Message::Close(code.map(|code| CloseFrame { code, reason: "".into() })))).await;
                    break;
                }
                message = receiver.next() => match message {
                    Some(Ok(Message::Binary(bytes))) => link.receive(Some(bytes.to_vec())),
                    Some(Ok(Message::Text(_))) => link.receive(None),
                    Some(Ok(Message::Ping(bytes))) => { if sender.send(Message::Pong(bytes)).await.is_err() {break;} },
                    Some(Ok(Message::Pong(_))) => {},
                    _ => break,
                },
                Some(Out::Bytes(bytes)) = outgoing.recv() => {
                    let len = bytes.len().max(1);
                    let result = tokio::time::timeout(std::time::Duration::from_secs(10),sender.send(Message::Binary(bytes.into()))).await;
                    port.buffered.fetch_sub(len,Ordering::AcqRel);
                    if !matches!(result,Ok(Ok(()))) {break;}
                }
            }
        }
        link.socket_closed();
    };
    (output, run)
}

/// TLS root certificate configuration for `wss://` WebSocket links.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WebSocketTlsConfig {
    /// Include operating-system native root CA certificates.
    pub use_native_roots: bool,
    /// Additional trusted root CA certificates in PEM format.
    pub extra_root_ca_pem: Vec<Vec<u8>>,
    /// Additional trusted root CA certificates in DER format.
    pub extra_root_ca_der: Vec<Vec<u8>>,
}

impl WebSocketTlsConfig {
    /// Trust the operating system's native root CA certificates.
    pub fn native_roots() -> Self {
        Self {
            use_native_roots: true,
            extra_root_ca_pem: Vec::new(),
            extra_root_ca_der: Vec::new(),
        }
    }

    /// Trust only the supplied PEM-encoded root CA certificate(s).
    pub fn with_custom_root_pem(pem: impl Into<Vec<u8>>) -> Self {
        Self {
            use_native_roots: false,
            extra_root_ca_pem: vec![pem.into()],
            extra_root_ca_der: Vec::new(),
        }
    }

    /// Trust the operating system's native root CAs plus the supplied PEM-encoded root CA(s).
    pub fn with_extra_root_pem(pem: impl Into<Vec<u8>>) -> Self {
        Self {
            use_native_roots: true,
            extra_root_ca_pem: vec![pem.into()],
            extra_root_ca_der: Vec::new(),
        }
    }
}

fn build_tls_connector(tls: &WebSocketTlsConfig) -> Result<tokio_tungstenite::Connector> {
    use rustls_pki_types::{CertificateDer, pem::PemObject};

    let mut roots = rustls::RootCertStore::empty();
    if tls.use_native_roots {
        let native = rustls_native_certs::load_native_certs();
        let (added, _ignored) = roots.add_parsable_certificates(native.certs);
        if added == 0 && tls.extra_root_ca_pem.is_empty() && tls.extra_root_ca_der.is_empty() {
            return Err(Error::Invalid(format!(
                "no native root CA certificates found (errors: {:?})",
                native.errors
            )));
        }
    }
    for pem in &tls.extra_root_ca_pem {
        let mut count = 0usize;
        for cert in CertificateDer::pem_slice_iter(pem) {
            let cert = cert.map_err(|e| Error::Invalid(format!("invalid PEM root CA: {e}")))?;
            roots
                .add(cert)
                .map_err(|e| Error::Invalid(format!("invalid root CA certificate: {e}")))?;
            count += 1;
        }
        if count == 0 {
            return Err(Error::Invalid(
                "PEM root CA buffer contained no certificates".into(),
            ));
        }
    }
    for der in &tls.extra_root_ca_der {
        roots
            .add(CertificateDer::from(der.clone()))
            .map_err(|e| Error::Invalid(format!("invalid DER root CA certificate: {e}")))?;
    }
    if roots.is_empty() {
        return Err(Error::Invalid(
            "TLS configuration has no trusted root certificates".into(),
        ));
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| Error::Invalid(format!("TLS protocol configuration failed: {e}")))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(tokio_tungstenite::Connector::Rustls(Arc::new(config)))
}

/// Open a real binary Chord transport over `ws://` or `wss://`. Credentials stay with the caller, not guests.
pub async fn connect_websocket(
    url: &str,
    headers: http::HeaderMap,
    max_payload: usize,
    max_buffered: usize,
) -> Result<Arc<dyn Link>> {
    connect_websocket_with_tls(url, headers, max_payload, max_buffered, None).await
}

/// Open a real binary Chord transport with explicit TLS root CA configuration for `wss://`.
pub async fn connect_websocket_with_tls(
    url: &str,
    headers: http::HeaderMap,
    max_payload: usize,
    max_buffered: usize,
    tls: Option<&WebSocketTlsConfig>,
) -> Result<Arc<dyn Link>> {
    use tokio_tungstenite::tungstenite::{
        client::IntoClientRequest,
        protocol::{CloseFrame, Message, WebSocketConfig, frame::coding::CloseCode},
    };
    let mut request = url
        .into_client_request()
        .map_err(|e| Error::Invalid(e.to_string()))?;
    request.headers_mut().extend(headers);
    let connector = if request
        .uri()
        .scheme_str()
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("wss"))
    {
        let owned;
        let tls_cfg = match tls {
            Some(cfg) => cfg,
            None => {
                owned = WebSocketTlsConfig::native_roots();
                &owned
            }
        };
        build_tls_connector(tls_cfg)?
    } else {
        tokio_tungstenite::Connector::Plain
    };
    let config = WebSocketConfig::default()
        .max_message_size(Some(max_payload))
        .max_frame_size(Some(max_payload));
    let (socket, _) = tokio_tungstenite::connect_async_tls_with_config(
        request,
        Some(config),
        false,
        Some(connector),
    )
    .await
    .map_err(|e| Error::Invalid(e.to_string()))?;
    let (link, port, mut outgoing) = port(max_payload, max_buffered);
    let output: Arc<dyn Link> = link.clone();
    tokio::spawn(async move {
        let mut closed = port.closed.subscribe();
        if closed.borrow().is_some() {
            closed.mark_changed();
        }
        let (mut sender, mut receiver) = socket.split();
        loop {
            tokio::select! { biased;
                _ = closed.changed() => {
                    let code = *closed.borrow_and_update();
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(1),sender.send(Message::Close(code.map(|code| CloseFrame { code:CloseCode::from(code), reason:"".into() })))).await;
                    break;
                }
                message = receiver.next() => match message {
                    Some(Ok(Message::Binary(bytes)))=>link.receive(Some(bytes.to_vec())),
                    Some(Ok(Message::Text(_)))=>link.receive(None),
                    Some(Ok(Message::Ping(bytes)))=>{if sender.send(Message::Pong(bytes)).await.is_err(){break;}},
                    Some(Ok(Message::Pong(_)))=>{},
                    _=>break,
                },
                Some(Out::Bytes(bytes)) = outgoing.recv()=>{
                    let len=bytes.len().max(1);
                    let result=tokio::time::timeout(std::time::Duration::from_secs(10),sender.send(Message::Binary(bytes.into()))).await;
                    port.buffered.fetch_sub(len,Ordering::AcqRel);
                    if !matches!(result,Ok(Ok(()))){break;}
                }
            }
        }
        link.socket_closed();
    });
    Ok(output)
}
