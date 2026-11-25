use axum::Router;
use std::path::Path;
use tokio::{net::TcpListener, task::JoinHandle};
use url::Url;

pub type Test<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub fn sandbox() -> Test<tempfile::TempDir> {
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
    std::fs::create_dir_all(&target)?;
    Ok(tempfile::tempdir_in(target)?)
}

pub struct Loopback {
    pub url: Url,
    task: JoinHandle<std::io::Result<()>>,
}
impl Loopback {
    pub async fn serve(router: Router) -> Test<Self> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let url = Url::parse(&format!("http://{}/", listener.local_addr()?))?;
        let task = tokio::spawn(async move { axum::serve(listener, router).await });
        Ok(Self { url, task })
    }
}
impl Drop for Loopback {
    fn drop(&mut self) {
        self.task.abort();
    }
}
