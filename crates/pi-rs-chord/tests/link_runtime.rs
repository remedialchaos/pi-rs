use pi_rs_chord::{
    Error, Result,
    link::{
        self, Body, Channel, ChannelHandler, ChannelOptions, Codec, IpcLink, IpcPort, Link,
        Message, WebSocketLink, WebSocketPort,
    },
    sync::SyncFuture,
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;
fn m(body: Body) -> Message {
    Message {
        body,
        origin: None,
        trace: None,
        old: false,
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn unhex(text: &str) -> Result<Vec<u8>> {
    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            u8::from_str_radix(
                std::str::from_utf8(pair).map_err(|_| Error::Invalid("hex".into()))?,
                16,
            )
            .map_err(|_| Error::Invalid("hex".into()))
        })
        .collect()
}
#[test]
fn text_all_layouts_match_untouched_ts_and_segment_framing_is_exact() -> Result<()> {
    let fixtures: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("fixtures/link-binary-upstream.json"))?;
    for fixture in fixtures {
        let wire = unhex(
            fixture["hex"]
                .as_str()
                .ok_or_else(|| Error::Invalid("fixture".into()))?,
        )?;
        let msg = link::decode(&wire)?;
        let text = link::encode_text(&msg)?;
        assert_eq!(
            hex(&text),
            fixture["textHex"].as_str().unwrap_or("missing"),
            "{}",
            fixture["kind"]
        );
        let decoded = link::decode_text(&text)?;
        assert_eq!(link::encode(&decoded)?, wire);
    }
    for bad in [
        b"[\"r\",1,[0,{}],[2]]\na".as_slice(),
        b"[\"p\"]\nx",
        b"[\"@\",1,[\"@\",2,[\"p\"]]]",
        b"[\"r\",1,[2,{}]]",
        b"[\"r\",1,[1,\"!\"]]",
        b"[\"s\",[[1,[],[],[-1]]]]",
        b"[\"p\",1]",
        b"[\"C\"]",
    ] {
        assert!(link::decode_text(bad).is_err(), "{:?}", bad);
    }
    Ok(())
}
struct Handler {
    opened: Notify,
    received: Arc<Mutex<Vec<u64>>>,
    gate: Arc<Notify>,
    started: Arc<Notify>,
    closes: AtomicUsize,
}
impl Handler {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            opened: Notify::new(),
            received: Arc::new(Mutex::new(Vec::new())),
            gate: Arc::new(Notify::new()),
            started: Arc::new(Notify::new()),
            closes: AtomicUsize::new(0),
        })
    }
}
impl ChannelHandler for Handler {
    fn opened(&self) {
        self.opened.notify_one();
    }
    fn closed(&self) {
        self.closes.fetch_add(1, Ordering::Relaxed);
    }
    fn message(&self, message: Message) -> SyncFuture<()> {
        let received = self.received.clone();
        let gate = self.gate.clone();
        let started = self.started.clone();
        Box::pin(async move {
            if let Body::Release { hold } = message.body {
                if hold == 1 {
                    started.notify_one();
                    gate.notified().await;
                }
                received
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(hold);
            }
            Ok(())
        })
    }
}
fn options(n: u8, errors: Arc<AtomicUsize>) -> ChannelOptions {
    ChannelOptions {
        codec: Codec::Binary,
        incarnation: [n; 16],
        calls: Some(2),
        report: Arc::new(move |_| {
            errors.fetch_add(1, Ordering::Relaxed);
        }),
    }
}
#[tokio::test]
async fn channel_hello_precedes_queue_and_async_dispatch_holds_pong_fences() -> Result<()> {
    let (a, b) = link::memory_link_pair();
    let ah = Handler::new();
    let bh = Handler::new();
    let errors = Arc::new(AtomicUsize::new(0));
    let ac = Channel::start(a, ah.clone(), options(1, errors.clone()))?;
    ac.send(m(Body::Release { hold: 1 }))?;
    ac.send(m(Body::Release { hold: 2 }))?;
    let bc = Channel::start(b, bh.clone(), options(2, errors.clone()))?;
    tokio::time::timeout(Duration::from_secs(1), bh.started.notified())
        .await
        .map_err(|_| Error::Invalid("handler did not start".into()))?;
    assert_eq!(ac.peer(), Some(([2; 16], Some(2))));
    assert!(
        bh.received
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    );
    let fence = {
        let ac = ac.clone();
        tokio::spawn(async move { ac.fence().await })
    };
    tokio::task::yield_now().await;
    assert!(!fence.is_finished());
    bh.gate.notify_one();
    tokio::time::timeout(Duration::from_secs(1), fence)
        .await
        .map_err(|_| Error::Invalid("fence stalled".into()))?
        .map_err(|e| Error::Invalid(e.to_string()))??;
    assert_eq!(
        *bh.received.lock().unwrap_or_else(|e| e.into_inner()),
        vec![1, 2]
    );
    ac.close();
    assert!(ac.closed());
    bc.close();
    assert_eq!(ah.closes.load(Ordering::Relaxed), 1);
    assert_eq!(bh.closes.load(Ordering::Relaxed), 1);
    assert_eq!(errors.load(Ordering::Relaxed), 0);
    Ok(())
}
#[tokio::test]
async fn greeting_timeout_and_protocol_error_close_once() -> Result<()> {
    let (a, b) = link::memory_link_pair();
    let h = Handler::new();
    let errors = Arc::new(AtomicUsize::new(0));
    let channel = Channel::start(a, h.clone(), options(1, errors.clone()))?;
    channel.tick(10000, 10000, 100, 1000)?;
    assert!(!channel.closed());
    channel.tick(11000, 0, 100, 1000)?;
    assert!(channel.closed());
    assert_eq!(h.closes.load(Ordering::Relaxed), 1);
    drop(b);
    let (a, b) = link::memory_link_pair();
    let h = Handler::new();
    let channel = Channel::start(a, h.clone(), options(1, errors.clone()))?;
    b.send(link::encode(&m(Body::Release { hold: 9 }))?)?;
    tokio::time::timeout(Duration::from_secs(1), async {
        while !channel.closed() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| Error::Invalid("protocol close stalled".into()))?;
    assert_eq!(errors.load(Ordering::Relaxed), 1);
    assert_eq!(h.closes.load(Ordering::Relaxed), 1);
    Ok(())
}
#[derive(Default)]
struct Socket {
    connecting: AtomicBool,
    buffer: AtomicUsize,
    sent: Mutex<Vec<Vec<u8>>>,
    codes: Mutex<Vec<u16>>,
    detached: AtomicUsize,
}
impl WebSocketPort for Socket {
    fn ready_state(&self) -> u8 {
        if self.connecting.load(Ordering::Relaxed) {
            0
        } else {
            1
        }
    }
    fn buffered_amount(&self) -> usize {
        self.buffer.load(Ordering::Relaxed)
    }
    fn send_binary(&self, b: Vec<u8>) -> Result<()> {
        self.sent.lock().unwrap_or_else(|e| e.into_inner()).push(b);
        Ok(())
    }
    fn close(&self, c: u16) {
        self.codes.lock().unwrap_or_else(|e| e.into_inner()).push(c);
    }
    fn detach_messages(&self) {
        self.detached.fetch_add(1, Ordering::Relaxed);
    }
}
#[test]
fn websocket_preserves_binary_frames_and_closes_on_text_or_backpressure() -> Result<()> {
    let socket = Arc::new(Socket::default());
    socket.connecting.store(true, Ordering::Relaxed);
    let link = WebSocketLink::new(socket.clone(), 4, 4);
    link.send(vec![1, 2])?;
    link.send(vec![3, 4])?;
    assert!(
        socket
            .sent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    );
    socket.connecting.store(false, Ordering::Relaxed);
    link.opened()?;
    assert_eq!(
        *socket.sent.lock().unwrap_or_else(|e| e.into_inner()),
        vec![vec![1, 2], vec![3, 4]]
    );
    link.receive(None);
    link.close();
    assert_eq!(
        *socket.codes.lock().unwrap_or_else(|e| e.into_inner()),
        vec![1002]
    );
    let socket = Arc::new(Socket::default());
    socket.buffer.store(4, Ordering::Relaxed);
    let link = WebSocketLink::new(socket.clone(), 4, 4);
    assert!(link.send(vec![1]).is_err());
    assert_eq!(
        *socket.codes.lock().unwrap_or_else(|e| e.into_inner()),
        vec![1009]
    );
    Ok(())
}
type Callback = Arc<dyn Fn(Result<()>) + Send + Sync>;
#[derive(Default)]
struct Port {
    done: Mutex<Option<Callback>>,
    sent: Mutex<Vec<Vec<u8>>>,
    detach: AtomicUsize,
}
impl IpcPort for Port {
    fn connected(&self) -> bool {
        true
    }
    fn send(&self, b: Vec<u8>, done: Callback) -> Result<bool> {
        self.sent.lock().unwrap_or_else(|e| e.into_inner()).push(b);
        *self.done.lock().unwrap_or_else(|e| e.into_inner()) = Some(done);
        Ok(false)
    }
    fn disconnect(&self) {}
    fn detach(&self) {
        self.detach.fetch_add(1, Ordering::Relaxed);
    }
}
#[test]
fn ipc_false_send_is_backpressure_and_callback_failure_closes() -> Result<()> {
    let port = Arc::new(Port::default());
    let link = IpcLink::new(port.clone());
    let messages = Arc::new(Mutex::new(Vec::new()));
    link.receive(None);
    link.receive(Some(vec![0, 255]));
    let out = messages.clone();
    link.on_message(Arc::new(move |b| {
        out.lock().unwrap_or_else(|e| e.into_inner()).push(b)
    }));
    assert_eq!(
        *messages.lock().unwrap_or_else(|e| e.into_inner()),
        vec![vec![0, 255]]
    );
    link.send(vec![9])?;
    assert!(!link.closed());
    let done = port
        .done
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .ok_or_else(|| Error::Invalid("missing callback".into()))?;
    done(Err(Error::Invalid("write failed".into())));
    assert!(link.closed());
    assert_eq!(port.detach.load(Ordering::Relaxed), 1);
    Ok(())
}
