//! Host-owned stream mirrors share one capture per store and source.
use super::{LogLevel, LogStore, LogWriter, Publication};
use std::{
    collections::HashMap,
    io::{self, Write},
    path::Path,
    sync::{Arc, Condvar, LazyLock, Mutex, Weak},
    thread,
};

#[derive(Default)]
struct Queue {
    bytes: Vec<u8>,
    discarded: bool,
    closed: bool,
}
type Pipe = Arc<(Mutex<Queue>, Condvar)>;
struct Stream {
    pipe: Pipe,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}
impl Stream {
    fn new(store: LogStore, source: String, level: LogLevel) -> io::Result<Self> {
        let pipe = Pipe::default();
        let reader = pipe.clone();
        let worker = thread::Builder::new().spawn(move || {
            let mut writer = LogWriter::new(store, source, level);
            loop {
                let (lock, ready) = &*reader;
                let queue = lock.lock().unwrap_or_else(|e| e.into_inner());
                let mut queue = ready
                    .wait_while(queue, |q| q.bytes.is_empty() && !q.discarded && !q.closed)
                    .unwrap_or_else(|e| e.into_inner());
                let bytes = std::mem::take(&mut queue.bytes);
                let discard = std::mem::take(&mut queue.discarded);
                let closed = queue.closed;
                drop(queue);
                if discard {
                    writer.discard()
                }
                writer.write(&bytes);
                if closed {
                    writer.end();
                    break;
                }
            }
        })?;
        Ok(Self {
            pipe,
            worker: Mutex::new(Some(worker)),
        })
    }
    fn enqueue(&self, data: &[u8]) {
        let (lock, ready) = &*self.pipe;
        let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
        if q.closed {
            return;
        }
        if data.len() > 262144 - q.bytes.len() {
            q.bytes.clear();
            q.discarded = true
        } else {
            q.bytes.extend_from_slice(data)
        }
        ready.notify_one();
    }
    fn stop(&self) {
        let (lock, ready) = &*self.pipe;
        lock.lock().unwrap_or_else(|e| e.into_inner()).closed = true;
        ready.notify_one();
        if let Some(t) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take()
            && t.thread().id() != thread::current().id()
        {
            let _ = t.join();
        }
    }
}
impl Drop for Stream {
    fn drop(&mut self) {
        self.stop()
    }
}
struct Shared {
    stdout: Stream,
    stderr: Stream,
}
type Key = (usize, String);
type Registry = Mutex<HashMap<Key, Weak<Shared>>>;
static CAPTURES: LazyLock<Registry> = LazyLock::new(Registry::default);
#[derive(Clone)]
pub struct ProcessCapture(Arc<Shared>);
impl ProcessCapture {
    pub fn acquire(store: LogStore, source: &str) -> io::Result<Self> {
        let key = (Arc::as_ptr(&store.0) as usize, source.to_owned());
        let mut registry = CAPTURES.lock().unwrap_or_else(|e| e.into_inner());
        registry.retain(|_, w| w.strong_count() > 0);
        if let Some(active) = registry.get(&key).and_then(Weak::upgrade) {
            return Ok(Self(active));
        }
        let active = Arc::new(Shared {
            stdout: Stream::new(store.clone(), source.into(), LogLevel::Stdout)?,
            stderr: Stream::new(store, source.into(), LogLevel::Stderr)?,
        });
        registry.insert(key, Arc::downgrade(&active));
        Ok(Self(active))
    }
    pub fn stdout<W: Write>(&self, output: W) -> impl Write {
        Mirror {
            output,
            capture: self.clone(),
            stderr: false,
        }
    }
    pub fn stderr<W: Write>(&self, output: W) -> impl Write {
        Mirror {
            output,
            capture: self.clone(),
            stderr: true,
        }
    }
}
struct Mirror<W> {
    output: W,
    capture: ProcessCapture,
    stderr: bool,
}
impl<W: Write> Write for Mirror<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let n = self.output.write(bytes)?;
        let shared = &self.capture.0;
        let stream = if self.stderr {
            &shared.stderr
        } else {
            &shared.stdout
        };
        stream.enqueue(&bytes[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

pub struct PublishedLogs {
    capture: ProcessCapture,
    store: LogStore,
    source: String,
    _publication: Publication,
}
impl PublishedLogs {
    pub fn start(store: LogStore, source: &str, directory: &Path) -> io::Result<Self> {
        Self::start_at(store, source, directory, &format!("{source}.logs.json"))
    }
    pub fn start_at(
        store: LogStore,
        source: &str,
        directory: &Path,
        file: &str,
    ) -> io::Result<Self> {
        store.add_directory(directory);
        let capture = ProcessCapture::acquire(store.clone(), source)?;
        let publication = Publication::start(store.clone(), source.into(), directory.join(file))?;
        Ok(Self {
            capture,
            store,
            source: source.into(),
            _publication: publication,
        })
    }
    pub fn streams(&self) -> &ProcessCapture {
        &self.capture
    }
    /// Drain both queues and publish the fatal diagnostic before returning to the host.
    pub fn fatal(self, error: &str) {
        self.capture.0.stdout.stop();
        self.capture.0.stderr.stop();
        self.store.append(&self.source, LogLevel::Error, error);
    }
}

impl PublishedLogs {
    /// Run the host inside the publication lifetime. Persist the failure before
    /// returning its error or resuming the original unwinding panic.
    pub fn run<T, E: std::fmt::Display>(
        self,
        host: impl FnOnce(&ProcessCapture) -> Result<T, E>,
    ) -> Result<T, E> {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| host(self.streams()))) {
            Ok(Err(error)) => {
                self.fatal(&error.to_string());
                Err(error)
            }
            Ok(result) => result,
            Err(payload) => {
                let message = payload
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| payload.downcast_ref::<&str>().copied())
                    .unwrap_or("host panicked");
                self.fatal(message);
                std::panic::resume_unwind(payload)
            }
        }
    }
}
