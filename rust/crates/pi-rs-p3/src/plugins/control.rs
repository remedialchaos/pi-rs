//! The control plugin on the server: the owner's control of the server over HTTP. `POST /_p3/stop`
//! stops it (as `SIGTERM` does; the command uses it, so it needs no signal); `POST /_p3/apply` builds
//! the changed plugins and rolls them out (the server, then the running workers).

use std::{sync::Arc, time::Duration};

use pi_rs_chord::{
    host::{PluginFacet, define_facet},
    node::http::{Http, HttpContribution, RouteOptions},
};
use serde_json::{Value, json};

use crate::{
    api::v1::contribute::PluginLives,
    http::{error, json},
    principals::Principal,
    runner::RolloutHost,
    server::ServerRuntime,
};

/// The path the owner posts to for the server to stop.
pub const STOP_PATH: &str = "/_p3/stop";

/// The path the owner posts to for changed plugins to be applied.
pub const APPLY_PATH: &str = "/_p3/apply";

pub fn facet(runtime: Arc<ServerRuntime>, http: Http, lives: &Arc<PluginLives>) -> PluginFacet {
    let lives = lives.clone();
    PluginFacet::new(
        "p3-control",
        define_facet(move |_host| -> pi_rs_chord::Result<()> {
            let live = lives.open("p3-control");
            let mut contribution = HttpContribution::new("p3-control");
            {
                let runtime = runtime.clone();
                contribution
                    .route(
                        &format!("POST {STOP_PATH}"),
                        RouteOptions::new(move |request| {
                            let runtime = runtime.clone();
                            Box::pin(async move {
                                if crate::http::principal(&request) != Some(Principal::Owner) {
                                    return error(403, "Forbidden");
                                }
                                // After the answer: the close drains running requests, this one written
                                // by then.
                                tokio::spawn(async move {
                                    tokio::time::sleep(Duration::from_millis(10)).await;
                                    runtime.stop();
                                });
                                json(200, &json!("stopping"))
                            })
                        }),
                    )
                    .map_err(|error| pi_rs_chord::Error::Invalid(error.to_string()))?;
            }
            {
                let runtime = runtime.clone();
                contribution
                    .route(
                        &format!("POST {APPLY_PATH}"),
                        RouteOptions::new(move |request| {
                            let runtime = runtime.clone();
                            Box::pin(async move {
                                if crate::http::principal(&request) != Some(Principal::Owner) {
                                    return error(403, "Forbidden");
                                }
                                let context = pi_rs_chord::context::Context::background();
                                match runtime.apply(&context).await {
                                    Ok(result) => json(200, &answer(&result)),
                                    Err(error) => crate::http::error(500, &error.to_string()),
                                }
                            })
                        }),
                    )
                    .map_err(|error| pi_rs_chord::Error::Invalid(error.to_string()))?;
            }
            http.contribute(contribution)
                .map_err(|error| pi_rs_chord::Error::Invalid(error.to_string()))?;
            {
                let http = http.clone();
                live.cleanup(move || http.withdraw("p3-control"));
            }
            Ok(())
        }),
    )
}

/// What an apply's answer says: each step's hosts, and the step that stopped it.
pub fn answer(result: &crate::runner::RolloutResult) -> Value {
    let steps = result
        .steps
        .iter()
        .map(|step| {
            json!({
                "name": step.name,
                "hosts": step.hosts.iter().map(shown).collect::<Vec<_>>(),
            })
        })
        .collect::<Vec<_>>();
    let stopped = result
        .stopped
        .clone()
        .map(Value::String)
        .unwrap_or(Value::Null);
    json!({
        "steps": steps,
        "stopped": stopped,
    })
}

/// What an apply's answer says of one host.
fn shown(host: &RolloutHost) -> Value {
    match host {
        RolloutHost::Reloaded { host, failed } => json!({ "host": host, "failed": failed }),
        RolloutHost::Stopped { host } => json!({ "host": host, "stopped": true }),
        RolloutHost::Failed { host, error } => json!({ "host": host, "error": error }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{RolloutResult, RolloutStep};

    #[test]
    fn an_apply_answer_names_every_step_and_its_hosts() {
        let result = RolloutResult {
            steps: vec![
                RolloutStep {
                    name: "server".into(),
                    hosts: vec![RolloutHost::Reloaded {
                        host: "p3-sessions".into(),
                        failed: vec!["p3-probe".into()],
                    }],
                },
                RolloutStep {
                    name: "workers".into(),
                    hosts: vec![RolloutHost::Failed {
                        host: "s1".into(),
                        error: "no".into(),
                    }],
                },
            ],
            stopped: Some("workers".into()),
        };
        assert_eq!(
            answer(&result),
            json!({
                "steps": [
                    {"name": "server", "hosts": [{"host": "p3-sessions", "failed": ["p3-probe"]}]},
                    {"name": "workers", "hosts": [{"host": "s1", "error": "no"}]}
                ],
                "stopped": "workers"
            })
        );
        let result = RolloutResult {
            steps: Vec::new(),
            stopped: None,
        };
        assert_eq!(answer(&result)["stopped"], Value::Null);
    }
}
