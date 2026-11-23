use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use indexmap::IndexMap;
use pi_rs_chord::{
    Error, Result,
    contracts::{Contract, LOCAL, schema},
    documents::Definition,
    host::{FacetHost, Members, ServeOptions},
    jobs::{JobContext, JobImplementation, JobOptions, Phase, RunningEntry, define_job},
    json::JsonValue,
    state::Source,
    watch::NativeWatchCall,
};
use pi_rs_durable::{
    EntryDraft,
    env::{
        ExecutionErrorCode, ShellCommand, ShellExecOptions, ShellOutputInfo, ShellOutputWindow,
        ShellSpillOptions,
    },
    harness::{SubmissionBody, SubmissionDraft},
};
use serde_json::{Value, json};
use tokio::sync::{Mutex, watch};

use crate::api::v1::worker::WorkerProcess;

use super::{
    attachments,
    conversation::{id_at, invalid, text},
};

const MAX_BYTES: usize = 50 * 1024;
const MAX_LINES: usize = 2000;
const SAMPLE: Duration = Duration::from_millis(200);

pub(super) fn serve(
    host: &mut FacetHost,
    contract: Arc<Contract>,
    worker: Arc<WorkerProcess>,
) -> Result<()> {
    let args = contract
        .current()
        .variants
        .get(LOCAL)
        .and_then(|variant| variant.methods.get("start"))
        .ok_or_else(|| Error::Invalid("Missing shell command schema".into()))?
        .args
        .schema
        .clone();
    use schema::{bool, num, obj, opt, required, str};
    let schema = obj([
        ("output", required(str())),
        ("truncated", required(bool())),
        ("running", required(bool())),
        ("cancelled", required(bool())),
        ("exitCode", opt(num())),
        ("fullOutputPath", opt(str())),
        ("error", opt(str())),
    ]);
    let mut options = JobOptions::new(2, args, schema::nul());
    options.state = Some(Arc::new(Definition::new(
        "p2.shell-commands.output",
        schema,
        1,
        || Ok(json!({"output":"","truncated":false,"running":true,"cancelled":false}).into()),
    )?));
    let definition = define_job("p2.shell-commands.run", options)?;
    host.serve(contract, move |_, _, object| {
        let implementation = worker.clone();
        let job = object.job(definition.clone(), JobImplementation::new(move |args, ctx| {
            let worker = implementation.clone();
            Box::pin(async move {
                worker.call(run(&worker, args.to_serde()?, ctx)).await?;
                Ok(JsonValue::Null)
            })
        }))?;
        let shells = Source::new(object.batch(), json!({}).into())?;
        let (running, live) = (job.clone(), shells.clone());
        object.on_live(move |ctx| {
            let (binding, live) = (running.clone(), live.clone());
            Box::pin(async move {
                let changed = live.clone();
                let observation = binding.get()?.running_source().observe(&ctx.context, move |event, context| {
                    if let NativeWatchCall::Value { value } = event {
                        changed.set(live_shells(&value)?.into(), context)?;
                    }
                    Ok(())
                }, |_| {})?;
                live.set(live_shells(&observation.value)?.into(), &ctx.context)?;
                std::future::pending::<()>().await;
                Ok(())
            })
        });
        let mut members = Members::new();
        members.state("shells", shells.clone())?;
        let admitted = Arc::new(Mutex::new(IndexMap::<String, ()>::new()));
        let start = job.clone();
        members.method("start", move |args, ctx| {
            let (start, admitted) = (start.clone(), admitted.clone());
            Box::pin(async move {
                let command: Value = args.to_serde()?;
                let request_id = text(&command, "requestId")?;
                let key = format!("{}:{request_id}", text(&command["author"], "id")?);
                let mut admitted = admitted.lock().await;
                if !admitted.contains_key(&key) {
                    let job = start.get()?;
                    if job.running().into_iter().any(|run| {
                        run.phase != Phase::Blocked && run.args.get("conversation") == args.get("conversation")
                    }) {
                        return Err(Error::Invalid("A bash command is already running. Press Esc to cancel it first.".into()));
                    }
                    job.start(args, None, &ctx.context).await?;
                    admitted.insert(key, ());
                    if admitted.len() > 100 {
                        admitted.shift_remove_index(0);
                    }
                }
                Ok(json!({"shellId":request_id}).into())
            })
        })?;
        members.method("abort", move |args, ctx| {
            let (job, shells) = (job.clone(), shells.clone());
            Box::pin(async move {
                let args: Value = args.to_serde()?;
                let shell_id = text(&args, "shellId")?;
                let job = job.get()?;
                let runs = job.running();
                let Some(run) = runs.iter().find(|run| running_shell(run, shell_id)) else {
                    return Ok(json!({"aborted":false}).into());
                };
                let (tx, mut cancelled) = watch::channel(false);
                let shell_id = shell_id.to_owned();
                let subscription = shells.subscribe(move |event, _| {
                    if let pi_rs_chord::watch::WatchCall::Value { value, .. } = event {
                        let value: Value = value.to_serde()?;
                        if !value.as_object().into_iter().flat_map(|items| items.values())
                            .any(|item| item["id"] == shell_id && item["running"] == true) {
                            tx.send_replace(true);
                        }
                    }
                    Ok(())
                }, |_| {});
                let abort = job.clone();
                let id = run.id.clone();
                let context = ctx.context.clone();
                let mut abort = tokio::spawn(async move { abort.abort(&id, &context).await });
                tokio::select! {
                    result = &mut abort => { result.map_err(invalid)??; }
                    result = cancelled.wait_for(|cancelled| *cancelled) => { result.map_err(invalid)?; }
                }
                drop(subscription);
                Ok(json!({"aborted":true}).into())
            })
        })?;
        Ok(members)
    }, ServeOptions::default())
}

fn running_shell(run: &RunningEntry, shell_id: &str) -> bool {
    run.phase != Phase::Blocked
        && run.args.get("requestId") == Some(&JsonValue::String(shell_id.into()))
        && run.state.as_ref().and_then(|state| state.get("running")) == Some(&JsonValue::Bool(true))
}

fn live_shells(runs: &[RunningEntry]) -> Result<Value> {
    let mut shells = serde_json::Map::new();
    for run in runs.iter().filter(|run| run.phase != Phase::Blocked) {
        let args: Value = run.args.to_serde()?;
        let Some(state) = &run.state else { continue };
        let mut state: serde_json::Map<String, Value> = serde_json::from_value(state.to_serde()?)?;
        state.remove("fullOutputPath");
        for (key, from) in [
            ("id", "requestId"),
            ("command", "command"),
            ("author", "author"),
            ("includeInContext", "includeInContext"),
        ] {
            state.insert(key.into(), args[from].clone());
        }
        shells.insert(
            id_at(&args, "conversation")?.to_string(),
            Value::Object(state),
        );
    }
    Ok(Value::Object(shells))
}

async fn run(worker: &WorkerProcess, command: Value, ctx: JobContext) -> Result<()> {
    let harness = worker.harness()?;
    let id = id_at(&command, "conversation")?;
    let script = text(&command, "command")?;
    let (writer, mut samples) = watch::channel((String::new(), false));
    let environment = harness.durable.build_env(id, &ctx.context).await;
    let ended = match environment {
        Ok(Some(environment)) => {
            let output = writer.clone();
            let mut callback =
                |text: &str, _: &pi_rs_chord::context::Context, info: ShellOutputInfo| {
                    output.send_modify(|(tail, truncated)| {
                        if info.skipped.is_some() {
                            tail.clear();
                            *truncated = true;
                        }
                        tail.push_str(text);
                        *truncated |= trim_output(tail);
                    });
                    Ok(())
                };
            let exec = environment.exec(
                ShellCommand::Script(script),
                Some(ShellExecOptions {
                    inherit_env: Some(true),
                    spill: Some(ShellSpillOptions {
                        after_bytes: MAX_BYTES,
                        after_lines: MAX_LINES,
                    }),
                    window: Some(ShellOutputWindow {
                        max_bytes: MAX_BYTES,
                        max_lines: MAX_LINES,
                        min_interval_ms: 200,
                        bytes_per_second: 100 * 1024,
                    }),
                    on_output: Some(&mut callback),
                    ..Default::default()
                }),
                &ctx.context,
            );
            tokio::pin!(exec);
            let mut sample = tokio::time::interval_at(tokio::time::Instant::now() + SAMPLE, SAMPLE);
            let result = loop {
                tokio::select! {
                    result = &mut exec => break result,
                    _ = sample.tick() => {
                        if samples.has_changed().map_err(invalid)? {
                            let (tail, truncated) = samples.borrow_and_update().clone();
                            ctx.change_state(|value| {
                                *value = json!({"output":tail,"truncated":truncated,"running":true,"cancelled":false}).into();
                                Ok(())
                            }).await?;
                        }
                    }
                }
            };
            match result {
                Ok(result) => (Some(result.exit_code), result.spill_path, None),
                Err(error) if error.code == ExecutionErrorCode::Aborted => (None, None, None),
                Err(error) => {
                    let message = if error.code == ExecutionErrorCode::Timeout {
                        "Command timed out".to_owned()
                    } else {
                        error.message
                    };
                    (None, error.spill_path, Some(message))
                }
            }
        }
        Ok(None) => (
            None,
            None,
            Some("Conversation environment is unavailable".to_owned()),
        ),
        Err(error) => (None, None, Some(error.to_string())),
    };
    let (tail, truncated) = samples.borrow().clone();
    let tail = tail
        .strip_suffix("\r\n")
        .or_else(|| tail.strip_suffix('\n'))
        .unwrap_or(&tail);
    let cancelled = ctx.context.is_aborted();
    let mut state = serde_json::Map::from_iter([
        ("output".to_owned(), json!(tail)),
        ("truncated".to_owned(), json!(truncated)),
        ("running".to_owned(), json!(false)),
        ("cancelled".to_owned(), json!(cancelled)),
    ]);
    if let Some(exit) = ended.0 {
        state.insert("exitCode".into(), json!(exit));
    }
    if let Some(path) = &ended.1 {
        state.insert("fullOutputPath".into(), json!(path));
    }
    if let Some(error) = &ended.2 {
        state.insert("error".into(), json!(error));
    }
    let mut data = state.clone();
    ctx.change_state(|value| {
        *value = Value::Object(state).into();
        Ok(())
    })
    .await?;
    data.remove("running");
    for key in ["author", "command", "includeInContext"] {
        data.insert(key.into(), command[key].clone());
    }
    let model = if command["includeInContext"] == true {
        let author = &command["author"];
        let name = attachments::escape(text(author, "name")?);
        let sender = if author["kind"] == "owner" {
            format!("<sender kind=\"owner\" name=\"{name}\"/>")
        } else {
            format!(
                "<sender kind=\"guest\" id=\"{}\" name=\"{name}\"/>",
                attachments::escape(text(author, "id")?)
            )
        };
        let body = if tail.is_empty() {
            "(no output)".to_owned()
        } else {
            format!("```\n{tail}\n```")
        };
        let mut body = format!("Ran `{script}`\n{body}");
        if cancelled {
            body.push_str("\n\n(command cancelled)");
        } else if let Some(error) = &ended.2 {
            body.push_str(&format!("\n\n{error}"));
        } else if let Some(code) = ended.0.filter(|code| *code != 0) {
            body.push_str(&format!("\n\nCommand exited with code {code}"));
        }
        if let Some(path) = ended.1.filter(|_| truncated) {
            body.push_str(&format!("\n\n[Output truncated. Full output: {path}]"));
        }
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(invalid)?
            .as_millis();
        Some(vec![
            json!({"role":"user","content":[{"type":"text","text":sender},{"type":"text","text":body}],"timestamp":time}),
        ])
    } else {
        None
    };
    let context = ctx.context.without_abort_signal();
    let target = harness
        .durable
        .conversation(id, &context)
        .await
        .map_err(invalid)?
        .ok_or_else(|| Error::Invalid(format!("Unknown conversation {id}")))?;
    let submission = target
        .submit(
            SubmissionDraft {
                request_id: Some(format!(
                    "{}:shell:{}",
                    text(&command["author"], "id")?,
                    ctx.job_id
                )),
                notify: None,
                body: SubmissionBody::Write {
                    entry: EntryDraft {
                        kind: "p2.shell".into(),
                        model,
                        data: Some(Value::Object(data)),
                        ..Default::default()
                    },
                },
            },
            &context,
        )
        .await
        .map_err(invalid)?;
    submission.wait(&context).await.map_err(invalid)?;
    Ok(())
}

fn trim_output(tail: &mut String) -> bool {
    let mut cut = tail
        .rmatch_indices('\n')
        .nth(MAX_LINES - 1)
        .map(|(index, _)| index + 1)
        .unwrap_or(0);
    if tail.len() - cut > MAX_BYTES {
        let bytes = tail.len() - MAX_BYTES;
        if let Some(next) = tail.as_bytes()[bytes..]
            .iter()
            .position(|byte| *byte == b'\n')
        {
            cut = bytes + next + 1;
        } else {
            cut = bytes;
            while !tail.is_char_boundary(cut) {
                cut += 1;
            }
        }
    }
    tail.drain(..cut);
    cut > 0
}
