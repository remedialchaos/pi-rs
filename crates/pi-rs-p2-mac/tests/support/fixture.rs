//! Test fixture: a child process with p2's observable contract (a `/health` endpoint, a `started`
//! file and a `term` file), so the supervisor tests can run a real child without a Node install.
//! It is not part of the app.

use std::path::PathBuf;

use axum::{Json, Router, routing::get};
use serde_json::json;

#[tokio::test]
async fn child() {
    if std::env::var_os("FIXTURE_STOP").is_none() {
        return;
    }
    let data = std::env::var_os("P2_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_default();
    let port = std::env::var("PORT")
        .ok()
        .and_then(|port| port.parse::<u16>().ok())
        .unwrap_or(0);
    let fake = std::env::var("P2_FAKE").unwrap_or_default();
    let cwd = std::env::current_dir().unwrap_or_default();
    let started = json!({
        "pid": std::process::id(),
        "fake": fake,
        "cwd": cwd.to_string_lossy(),
    });
    if !data.as_os_str().is_empty() {
        let _ = std::fs::write(data.join("started"), started.to_string());
    }
    let app = Router::new().route("/health", get(|| async { Json(json!({"name":"p2"})) }));
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("p2 fixture could not bind port {port}: {error}");
            std::process::exit(1);
        }
    };
    let graceful = std::env::var("FIXTURE_STOP").as_deref() == Ok("graceful");
    let signal_data = data.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            if let Ok(mut terminate) = signal(SignalKind::terminate()) {
                terminate.recv().await;
                if !signal_data.as_os_str().is_empty() {
                    let _ = std::fs::write(signal_data.join("term"), "received");
                }
                if graceful {
                    std::process::exit(0);
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (signal_data, graceful);
        }
    });
    if let Err(error) = axum::serve(listener, app).await {
        eprintln!("p2 fixture stopped: {error}");
        std::process::exit(1);
    }
}
