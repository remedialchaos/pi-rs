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
        self.buffered
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                n.checked_add(len).filter(|n| *n <= self.maximum)
            })
            .map_err(|_| Error::Invalid("websocket output exceeds bound".into()))?;
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

/// Open a real binary Chord transport. Credentials stay with the caller, not guests.
pub async fn connect_websocket(
    url: &str,
    headers: http::HeaderMap,
    max_payload: usize,
    max_buffered: usize,
) -> Result<Arc<dyn Link>> {
    use tokio_tungstenite::tungstenite::{
        client::IntoClientRequest,
        protocol::{CloseFrame, Message, WebSocketConfig, frame::coding::CloseCode},
    };
    let mut request = url
        .into_client_request()
        .map_err(|e| Error::Invalid(e.to_string()))?;
    request.headers_mut().extend(headers);
    let config = WebSocketConfig::default()
        .max_message_size(Some(max_payload))
        .max_frame_size(Some(max_payload));
    let (socket, _) = tokio_tungstenite::connect_async_with_config(request, Some(config), false)
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
