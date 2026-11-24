use super::{DevCommand, DevResult, read_command};
use axum::{
    Json,
    body::{Body, to_bytes},
    response::IntoResponse,
};
use http::{Method, StatusCode};
use sha2::{Digest, Sha256};
use std::{
    fs, io,
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    net::{UnixListener, UnixStream},
    task::JoinHandle,
};

struct OwnedPath {
    path: PathBuf,
    inode: u64,
}
impl Drop for OwnedPath {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path).is_ok_and(|m| m.ino() == self.inode) {
            let _ = fs::remove_file(&self.path);
        }
    }
}
pub struct Channel {
    stop: tokio::sync::oneshot::Sender<()>,
    task: JoinHandle<()>,
}
impl Channel {
    pub async fn close(self) {
        let _ = self.stop.send(());
        let _ = self.task.await;
    }
}

pub async fn serve<F, Fut>(directory: &Path, execute: F) -> io::Result<Channel>
where
    F: Fn(DevCommand) -> Fut + Send + Sync + Clone + 'static,
    Fut: Future<Output = DevResult> + Send + 'static,
{
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    let dir = directory.canonicalize()?;
    let name = format!("p2:dev-control:{}", dir.display());
    let digest = format!("{:x}", Sha256::digest(name.as_bytes()));
    let key = format!("chord-lock-{}", &digest[..32]);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let lock = {
        #[cfg(target_os = "android")]
        use std::os::android::net::SocketAddrExt;
        #[cfg(target_os = "linux")]
        use std::os::linux::net::SocketAddrExt;
        use std::os::unix::net::{SocketAddr, UnixListener};
        UnixListener::bind_addr(&SocketAddr::from_abstract_name(key)?).map_err(|err| {
            if err.kind() == io::ErrorKind::AddrInUse {
                io::Error::new(err.kind(), format!("{name} is held by another process"))
            } else {
                err
            }
        })?
    };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let lock = {
        let path = std::env::temp_dir().join(format!("{key}.sock"));
        if path.as_os_str().as_encoded_bytes().len() > 103 {
            return Err(io::Error::other(
                "Temporary directory path is too long for dev control lock",
            ));
        }
        let listener = std::os::unix::net::UnixListener::bind(&path)?;
        let owned = OwnedPath {
            inode: fs::symlink_metadata(&path)?.ino(),
            path,
        };
        (listener, owned)
    };
    let path = dir.join("dev.sock");
    match fs::symlink_metadata(&path) {
        Ok(m) if m.file_type().is_socket() => match UnixStream::connect(&path).await {
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => fs::remove_file(&path)?,
            Err(e) => return Err(e),
            Ok(_) => {
                return Err(io::Error::other(format!(
                    "{name} is held by another process"
                )));
            }
        },
        Ok(_) => return Err(io::Error::other("Dev control path is not a Unix socket")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => (),
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(&path)?;
    let owned = OwnedPath {
        inode: fs::symlink_metadata(&path)?.ino(),
        path,
    };
    fs::set_permissions(&owned.path, fs::Permissions::from_mode(0o600))?;
    let service = hyper::service::service_fn(move |req: http::Request<hyper::body::Incoming>| {
        let execute = execute.clone();
        async move {
            let parsed = if req.method() != Method::POST || req.uri() != "/" {
                Err("POST / required")
            } else {
                match tokio::time::timeout(
                    Duration::from_secs(120),
                    to_bytes(Body::new(req.into_body()), 16384),
                )
                .await
                {
                    Ok(Ok(body)) => read_command(&body),
                    Ok(Err(_)) => Err("Dev command too large"),
                    Err(_) => Err("Dev command timed out"),
                }
            };
            let response = match parsed {
                Ok(cmd) => {
                    // Closing the requesting socket must not cancel a reload halfway through.
                    let result =
                        tokio::spawn(execute(cmd))
                            .await
                            .unwrap_or_else(|error| DevResult {
                                ok: false,
                                message: error.to_string(),
                                plugins: None,
                            });
                    (
                        if result.ok {
                            StatusCode::OK
                        } else {
                            StatusCode::CONFLICT
                        },
                        Json(result),
                    )
                }
                Err(message) => (
                    StatusCode::BAD_REQUEST,
                    Json(DevResult {
                        ok: false,
                        message: message.into(),
                        plugins: None,
                    }),
                ),
            };
            Ok::<_, std::convert::Infallible>(response.into_response())
        }
    });
    let (stop, mut stopping) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let _lock = lock;
        let _owned = owned;
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                biased;
                _ = &mut stopping => break,
                Some(_) = connections.join_next(), if !connections.is_empty() => {},
                accepted = listener.accept() => {
                    let Ok((socket, _)) = accepted else { break };
                    let service = service.clone();
                    connections.spawn(async move {
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(hyper_util::rt::TokioIo::new(socket), service).await;
                    });
                }
            }
        }
        drop(listener);
        connections.shutdown().await;
    });
    Ok(Channel { stop, task })
}

pub async fn send(directory: &Path, command: &DevCommand) -> io::Result<DevResult> {
    tokio::time::timeout(Duration::from_secs(120), async {
        let socket = UnixStream::connect(directory.join("dev.sock")).await?;
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(socket))
                .await
                .map_err(io::Error::other)?;
        tokio::spawn(connection);
        let req = http::Request::post("/")
            .header("host", "localhost")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(command)?))
            .map_err(io::Error::other)?;
        let res = sender.send_request(req).await.map_err(io::Error::other)?;
        let body = to_bytes(Body::new(res.into_body()), 256 * 1024)
            .await
            .map_err(|_| io::Error::other("Dev result too large"))?;
        serde_json::from_slice(&body).map_err(|_| io::Error::other("Invalid dev result"))
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Dev command timed out"))?
}
