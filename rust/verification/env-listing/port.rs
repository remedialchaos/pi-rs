//! Exercise the public daemon-backed listing adapters with one live connection per scenario.
#![forbid(unsafe_code)]

use pi_rs_chord::context::Context;
use pi_rs_durable::env::{ExecutionEnv, ListFilesOptions, LocalEnvOptions};
use pi_rs_env::{
    Connection, ConnectionOptions, LocalExecutionEnv, RemoteExecutionEnv, RemoteExecutionEnvOptions,
};
use serde_json::{Value, json};
use std::{env, sync::Arc};

pub async fn run_listing(remote: bool) -> Result<Value, String> {
    let fixture = env::var("LISTING_FIXTURE").map_err(|e| e.to_string())?;
    let cwd = "/".to_owned();
    let daemon = env::var("PI_ENV_DAEMON").map_err(|e| e.to_string())?;
    let connection = Arc::new(Connection::new(ConnectionOptions::new(vec![daemon])));
    let env: Box<dyn ExecutionEnv> = if remote {
        Box::new(RemoteExecutionEnv::new(RemoteExecutionEnvOptions {
            connection: connection.clone(),
            id: "pi-env:proof".into(),
            cwd,
            shell_env: Default::default(),
            shell_path: None,
            watch: Default::default(),
        }))
    } else {
        Box::new(LocalExecutionEnv::new(
            LocalEnvOptions {
                cwd,
                ..Default::default()
            },
            connection.clone(),
        ))
    };
    let context = Context::background();
    let (aborted, cancel) = context.with_cancel();
    cancel.cancel("aborted");
    let mut results = Vec::new();
    for (path, max_entries, ctx) in [
        (fixture.as_str(), None, &context),
        (fixture.as_str(), Some(1), &context),
        ("/etc/hosts", None, &context),
        (".", Some(1 << 53), &context),
        (".", None, &aborted),
    ] {
        let options = ListFilesOptions {
            max_entries,
            ..Default::default()
        };
        results.push(match env.list_files(path, Some(options), ctx).await {
            Ok(value) => json!({"ok": true, "value": value}),
            Err(error) => json!({"ok": false, "error": error}),
        });
    }
    connection.close();
    Ok(Value::Array(results))
}
