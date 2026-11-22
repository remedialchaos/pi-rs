use crate::{Error, Result};
use std::path::Path;

pub fn available(data_dir: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        data_dir
            .join("dev.sock")
            .metadata()
            .is_ok_and(|m| m.file_type().is_socket())
    }
    #[cfg(not(unix))]
    {
        let _ = data_dir;
        false
    }
}

pub async fn restart(data_dir: &Path) -> Result<String> {
    #[cfg(unix)]
    {
        let client = reqwest::Client::builder()
            .no_proxy()
            .unix_socket(data_dir.join("dev.sock"))
            .timeout(std::time::Duration::from_secs(120))
            .build()?;
        let answer = client
            .post("http://localhost/")
            .header(reqwest::header::CONNECTION, "close")
            .json(&serde_json::json!({"action":"restart"}))
            .send()
            .await?;
        let status = answer.status();
        #[derive(serde::Deserialize)]
        struct Reply {
            ok: bool,
            message: String,
        }
        let reply = answer.json::<Reply>().await?;
        if !status.is_success() || !reply.ok {
            return Err(Error::Protocol(reply.message));
        }
        Ok(reply.message)
    }
    #[cfg(not(unix))]
    {
        let _ = data_dir;
        Err(Error::Protocol(
            "dev.sock is unavailable on this platform".into(),
        ))
    }
}
