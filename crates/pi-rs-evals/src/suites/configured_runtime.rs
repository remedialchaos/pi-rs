//! Inspect the configured catalog and actually dispatch the fixture request through Pi.
use super::*;
use pi_rs_ai::{Model, ModelInputModality};
use pi_rs_coding_agent::core::model_registry::ModelRegistry;
use pi_rs_coding_agent::modes::rpc::RpcClient;
use std::process::Stdio;
use std::time::Duration;

fn model_matches(
    m: &Model,
    provider: &str,
    id: &str,
    name: &str,
    reasoning: bool,
    context: u64,
    max: u64,
) -> bool {
    m.id == id
        && m.name == name
        && m.provider == provider
        && m.reasoning == reasoning
        && m.input == [ModelInputModality::Text]
        && m.cost.rates.input == 0.0
        && m.cost.rates.output == 0.0
        && m.cost.rates.cache_read == 0.0
        && m.cost.rates.cache_write == 0.0
        && m.context_window == context
        && m.max_tokens == max
}
// Start the oracle with a built-in offline model so a missing subject-created model is
// observable through the catalog and remains an ordinary failed assertion.
pub(super) fn probe(
    pi: &Path,
    ws: &EvalWorkspace,
) -> EvalResult<(tokio::process::Child, RpcClient)> {
    let mut command = ws.command(pi, &BTreeMap::new())?;
    command
        .args([
            "--mode",
            "rpc",
            "--approve",
            "--offline",
            "--no-session",
            "--no-tools",
            "--provider",
            "openai",
            "--model",
            "gpt-5.4",
        ])
        .env("OPENAI_API_KEY", "local-offline-catalog-only")
        .env("ACME_API_KEY", "resolved-acme-key")
        .env("ACME_STREAM_API_KEY", "resolved-stream-key")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut process = command.spawn()?;
    let rpc = RpcClient::from_stream(
        process
            .stdout
            .take()
            .ok_or_else(|| EvalError::message("Missing Pi RPC stdout"))?,
        process
            .stdin
            .take()
            .ok_or_else(|| EvalError::message("Missing Pi RPC stdin"))?,
    );
    Ok((process, rpc))
}
pub(super) async fn added_model(pi: &Path, ws: &EvalWorkspace) -> EvalResult<bool> {
    let original = ModelRegistry::with_builtin(Default::default(), None)
        .map_err(EvalError::message)?
        .list_for_provider("openai");
    let (_child, rpc) = probe(pi, ws)?;
    let available = tokio::time::timeout(Duration::from_secs(30), rpc.get_available_models())
        .await
        .map_err(|_| EvalError::message("Model catalog probe timed out"))??;
    Ok(!original.is_empty()
        && original.iter().all(|m| {
            available
                .iter()
                .any(|other| other.provider == "openai" && other.id == m.id)
        })
        && available.iter().any(|m| {
            model_matches(
                m,
                "openai",
                "fixture-chat",
                "Fixture Chat",
                true,
                32768,
                4096,
            )
        }))
}
pub(super) async fn provider(
    pi: &Path,
    ws: &EvalWorkspace,
    fixture: &acme_server::Server,
) -> EvalResult<bool> {
    let (mut process, mut rpc) = probe(pi, ws)?;
    let result = tokio::time::timeout(Duration::from_secs(45), async {
        let available = rpc.get_available_models().await?;
        let Some(model) = available
            .into_iter()
            .find(|m| m.provider == fixture.provider() && m.id == fixture.model())
        else {
            return Ok(false);
        };
        let (name, context, max, answer, input_tokens, output_tokens) = if fixture.stream {
            ("Acme Stream Chat", 16384, 2048, "ACME_STREAM_OK", 4, 3)
        } else {
            ("Acme Chat", 32768, 4096, "ACME_OK", 3, 2)
        };
        if !model_matches(
            &model,
            fixture.provider(),
            fixture.model(),
            name,
            false,
            context,
            max,
        ) || (!fixture.stream
            && model.base_url.trim_end_matches('/') != format!("{}/v1", fixture.origin))
        {
            return Ok(false);
        }
        rpc.set_model(fixture.provider(), fixture.model()).await?;
        fixture.reset();
        rpc.prompt_and_wait(
            format!("Reply with {answer}."),
            vec![],
            Duration::from_secs(30),
        )
        .await?;
        let messages = serde_json::to_value(rpc.get_messages().await?)?;
        let last = messages
            .as_array()
            .and_then(|m| m.iter().rev().find(|v| v["role"] == "assistant"));
        Ok(last.is_some_and(|m| {
            m["stopReason"] == "stop"
                && m["usage"]["input"] == input_tokens
                && m["usage"]["output"] == output_tokens
                && m["content"].as_array().is_some_and(|parts| {
                    parts
                        .iter()
                        .filter_map(|p| p["text"].as_str())
                        .collect::<String>()
                        == answer
                })
        }) && fixture.valid())
    })
    .await;
    rpc.stop().await?;
    if process.try_wait()?.is_none() {
        process.kill().await?;
    }
    result.map_err(|_| EvalError::message("Configured provider probe timed out"))?
}
