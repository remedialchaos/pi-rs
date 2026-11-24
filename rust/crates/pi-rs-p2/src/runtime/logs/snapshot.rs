use super::LogStore;
use std::{
    fs::{DirBuilder, OpenOptions},
    io::{self, Write},
    path::PathBuf,
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};

pub struct Publication {
    store: LogStore,
    subscription: u64,
    sender: Option<mpsc::SyncSender<()>>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Publication {
    pub fn start(store: LogStore, source: String, path: PathBuf) -> io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel(1);
        let publish_store = store.clone();
        let monitored = source.clone();
        let wake = tx.clone();
        let subscription = store.subscribe(Arc::new(move |s| {
            if s == monitored {
                let _ = wake.try_send(());
            }
        }));
        let worker = thread::Builder::new().spawn(move || {
            let write = || -> io::Result<()> {
                let mut dir = DirBuilder::new();
                dir.recursive(true);
                let mut opts = OpenOptions::new();
                opts.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
                    dir.mode(0o700);
                    opts.mode(0o600);
                }
                if let Some(parent) = path.parent() {
                    dir.create(parent)?;
                }
                let tmp = path.with_added_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
                let mut file = opts.open(&tmp)?;
                let result = (|| {
                    serde_json::to_writer(
                        &mut file,
                        &serde_json::json!({"source":source,"lines":publish_store.lines(&source)}),
                    )?;
                    file.flush()?;
                    drop(file);
                    std::fs::rename(&tmp, &path)
                })();
                if result.is_err() {
                    let _ = std::fs::remove_file(tmp);
                }
                result
            };
            let _ = write();
            while rx.recv().is_ok() {
                thread::sleep(Duration::from_millis(50));
                let _ = rx.try_recv();
                let _ = write();
            }
            let _ = write();
        });
        match worker {
            Ok(worker) => Ok(Self {
                store,
                subscription,
                sender: Some(tx),
                worker: Some(worker),
            }),
            Err(e) => {
                store.unsubscribe(subscription);
                Err(e)
            }
        }
    }
}
impl Drop for Publication {
    fn drop(&mut self) {
        self.store.unsubscribe(self.subscription);
        self.sender.take();
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}
