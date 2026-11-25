use std::{collections::HashMap, sync::Arc};

use pi_rs_ai::{InputContent, Tool};
use pi_rs_chord::{
    Result,
    context::Context,
    host::{Facet, Held, define_facet},
    output::{OutputLimits, Retain},
};
use pi_rs_durable::{
    harness::{Extension, Invocation, ToolRegistration},
    tools::ToolResult,
};
use serde_json::{Value, json};

use crate::{
    api::v1::p2_contracts::lookup,
    hosts::worker::{WorkerProcess, worker_of},
    runtime::{
        location::{is_rooted, join_path, location_text},
        session_worker::Location,
        worker_environment::{conversation_confinement, locate_conversation},
    },
};

use super::service::invalid;

const COMMANDS: [&str; 6] = [
    "new-window",
    "list-windows",
    "send-keys",
    "read",
    "capture-pane",
    "kill-window",
];

pub(super) fn facet() -> Result<Facet> {
    let contract = Arc::new(lookup("p2.agent-terminals")?);
    Ok(define_facet(move |host| {
        let worker = worker_of(&host.host())?;
        let host = host.host().downgrade();
        let contract = contract.clone();
        let execute_worker = Arc::downgrade(&worker);
        let extension = Extension {
            name:"p2-terminals".into(),
            tools:vec![ToolRegistration {
                declaration:Tool {
                    name:"terminal".into(),
                    description:"Use the session's terminals when the user asks for a terminal or to drive an interactive program. Commands accept tmux argv: new-window [-n name] [-c directory] [-w wait] [-T seconds] [command], list-windows, send-keys -t target [-l] keys, read -t target, capture-pane -t target [-S -lines] [-e], kill-window -t target. Waits accept quiet, exit, none, or /pattern/i.".into(),
                    parameters:json!({"type":"object","required":["command"],"additionalProperties":false,"properties":{"command":{"enum":COMMANDS},"args":{"type":"array","items":{"type":"string"}}}}),
                    constrained_sampling:None,
                },
                deferred:true,
                callers:if worker.registries.extensions.snapshot().extension(pi_rs_durable::codemode::CODEMODE_TOOL_NAME).is_some() {
                    Some(vec!["tools".into()])
                } else { None },
                sequential:false,
                replay_safe:true,
                prepare_arguments:None,
                cancel:None,
                structured_output_schema:Some(result_schema()),
                output_limits:Some(OutputLimits { max_bytes:17 * 1024, retain:Retain::Tail, max_lines:usize::MAX }),
                execute:Arc::new(move |args,invocation,ctx| {
                    let (host,contract,worker) = (host.clone(),contract.clone(),execute_worker.clone());
                    Box::pin(async move {
                        let result = async {
                            let worker = worker.upgrade().ok_or(pi_rs_chord::Error::Closed)?;
                            let host = host.upgrade().ok_or(pi_rs_chord::Error::Closed)?;
                            let backend = host.get(contract,json!({}).into(),ctx).await?;
                            execute(&worker,&backend,args,invocation,ctx).await
                        }.await;
                        match result {
                            Ok(value) => Ok(value.into()),
                            Err(error) if ctx.check().is_err() => Err(pi_rs_durable::session::Error::invalid(error.to_string())),
                            Err(error) => Ok(ToolResult {
                                output:Some(vec![InputContent::Text(error.to_string().into())]),
                                is_error:Some(true),
                                ..Default::default()
                            }.into()),
                        }
                    })
                }),
            }],
            ..Default::default()
        };
        worker.registries.extensions.install(extension)?;
        Ok(())
    }))
}

fn result_schema() -> Value {
    let mut state = json!({"window":{"type":"integer"},"running":{"type":"boolean"},"foreground":{"type":"string"},"exitCode":{"type":"integer"},"error":{"type":"string"}});
    let window = json!({
        "type":"object", "additionalProperties":false,
        "required":["window","location","opener","running"],
        "properties":{
            "window":state["window"],"running":state["running"],"foreground":state["foreground"],
            "exitCode":state["exitCode"],"error":state["error"],"name":{"type":"string"},"title":{"type":"string"},
            "location":{"type":"string"},"opener":{"enum":["user","agent"]}
        },
    });
    state["output"] = json!({"type":"string"});
    state["screen"] = json!({"type":"boolean"});
    state["skipped"] = json!({"type":"integer"});
    state["timedOut"] = json!({"type":"boolean"});
    state["killed"] = json!({"type":"boolean"});
    state["windows"] = json!({"type":"array","items":window});
    json!({"type":"object","additionalProperties":false,"properties":state})
}

async fn call(backend: &Held, method: &str, args: Value, ctx: &Context) -> Result<Value> {
    backend.call(method, args.into(), ctx).await?.to_serde()
}

struct Arguments<'a> {
    options: HashMap<char, &'a str>,
    rest: &'a [String],
}

impl<'a> Arguments<'a> {
    fn parse(command: &str, args: &'a [String]) -> Result<Self> {
        let (flags, values) = match command {
            "new-window" => ("", "ncwT"),
            "list-windows" => ("", ""),
            "send-keys" => ("l", "twT"),
            "read" => ("", "twT"),
            "capture-pane" => ("ep", "tS"),
            "kill-window" => ("", "t"),
            _ => return Err(invalid("Unknown terminal command")),
        };
        let mut options = HashMap::new();
        let mut index = 0;
        while let Some(arg) = args.get(index) {
            if arg == "--" {
                index += 1;
                break;
            }
            let Some(options_text) = arg.strip_prefix('-').filter(|text| !text.is_empty()) else {
                break;
            };
            index += 1;
            for (at, flag) in options_text.char_indices() {
                if flags.contains(flag) {
                    options.insert(flag, "");
                } else if values.contains(flag) {
                    let value = &options_text[at + flag.len_utf8()..];
                    let value = if value.is_empty() {
                        let value = args.get(index).ok_or_else(|| {
                            invalid(format!("{command}: option requires an argument -- {flag}"))
                        })?;
                        index += 1;
                        value.as_str()
                    } else {
                        value
                    };
                    options.insert(flag, value);
                    break;
                } else {
                    return Err(invalid(format!("{command}: unknown option -- {flag}")));
                }
            }
        }
        let rest = &args[index..];
        if (command == "send-keys" && rest.is_empty())
            || (!matches!(command, "new-window" | "send-keys") && !rest.is_empty())
        {
            return Err(invalid(format!("Invalid arguments to {command}")));
        }
        Ok(Self { options, rest })
    }

    fn target(&self) -> Result<&str> {
        self.options
            .get(&'t')
            .copied()
            .filter(|target| !target.is_empty())
            .ok_or_else(|| invalid("Missing target window (-t)"))
    }

    fn wait(&self, default: &str, seconds: f64) -> Result<Value> {
        let timeout = self
            .options
            .get(&'T')
            .map(|s| s.parse::<f64>().map_err(invalid))
            .transpose()?
            .unwrap_or(seconds);
        if !(0. ..=120.).contains(&timeout) {
            return Err(invalid("Invalid terminal timeout"));
        }
        let timeout = (timeout * 1000.).round() as u64;
        let wait = self.options.get(&'w').copied().unwrap_or(default);
        Ok(match wait {
            "none" => Value::Null,
            "quiet" | "exit" => json!({"until":wait,"timeoutMs":timeout}),
            pattern if pattern.starts_with('/') => {
                let (pattern, flag) = pattern[1..]
                    .rsplit_once('/')
                    .ok_or_else(|| invalid("Invalid terminal wait pattern"))?;
                if !matches!(flag, "" | "i") {
                    return Err(invalid("Invalid terminal wait pattern"));
                }
                json!({"until":"match","pattern":pattern,"ignoreCase":flag=="i","timeoutMs":timeout})
            }
            _ => return Err(invalid("Invalid terminal wait")),
        })
    }
}

async fn execute(
    worker: &WorkerProcess,
    backend: &Held,
    value: Value,
    invocation: &Invocation,
    ctx: &Context,
) -> Result<ToolResult> {
    let command = value["command"]
        .as_str()
        .ok_or_else(|| invalid("Missing terminal command"))?;
    let args: Vec<String> = value
        .get("args")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();
    let parsed = Arguments::parse(command, &args)?;
    let id = invocation.record.conversation_id;
    let key = invocation.record.id.to_string();
    let harness = worker.harness()?;
    let (mut here, sandbox) = {
        let execution = harness.environments.lock().await;
        let here = locate_conversation(Some(&execution), id, ctx, None)
            .await
            .map_err(invalid)?;
        let sandbox = conversation_confinement(&execution, id, ctx, None)
            .await
            .map_err(invalid)?
            .map(|rule| rule.sandbox);
        (here, sandbox)
    };
    let reachable = |terminal: &Value| -> Result<()> {
        if let Some(sandbox) = &sandbox
            && terminal["env"].as_str() != Some(sandbox)
        {
            return Err(invalid(format!(
                "That window runs outside sandbox {sandbox}, where you work"
            )));
        }
        Ok(())
    };
    let mut details = json!({"command":command,"args":args});
    let result = if command == "list-windows" {
        let listed = call(backend, "list", json!({}), ctx).await?;
        let mut windows = Vec::new();
        if let Some(entries) = listed["windows"].as_array() {
            for window in entries {
                let terminal = &window["terminal"];
                if reachable(terminal).is_err() {
                    continue;
                }
                let location: Location =
                    serde_json::from_value(json!({"cwd":terminal["cwd"],"env":terminal["env"]}))?;
                let mut row = state(terminal, &window["foreground"]);
                row["location"] = json!(location_text(&location));
                if terminal["title"]
                    .as_str()
                    .is_some_and(|title| !title.is_empty())
                {
                    row["title"] = terminal["title"].clone();
                }
                row["opener"] = json!(if terminal["agent"] == true {
                    "agent"
                } else {
                    "user"
                });
                if let Some(name) = terminal.get("name") {
                    row["name"] = name.clone();
                }
                windows.push(row);
            }
        }
        json!({"windows":windows})
    } else {
        let wait = match command {
            "new-window" => parsed.wait("quiet", 10.)?,
            "send-keys" => parsed.wait("quiet", 5.)?,
            "read" => parsed.wait("none", 10.)?,
            _ => Value::Null,
        };
        let (terminal, offset) = if command == "new-window" {
            if let Some(dir) = parsed.options.get(&'c').filter(|dir| !dir.is_empty()) {
                here.cwd = if is_rooted(dir) {
                    (*dir).into()
                } else {
                    join_path(&here.cwd, dir)
                };
            }
            let mut request = json!({"key":key,"location":here});
            if let Some(name) = parsed.options.get(&'n').filter(|name| !name.is_empty()) {
                request["name"] = json!(name);
            }
            if !parsed.rest.is_empty() {
                request["command"] = json!(parsed.rest.join(" "));
            }
            (call(backend, "open", request, ctx).await?, json!(0))
        } else if command == "capture-pane" {
            let value = call(backend, "cursor", json!({"target":parsed.target()?}), ctx).await?;
            (value["terminal"].clone(), value["offset"].clone())
        } else {
            let memo_key = if command == "kill-window" {
                "window"
            } else {
                "start"
            };
            let mut remembered = invocation
                .memo(memo_key, None, ctx)
                .await
                .map_err(invalid)?;
            if remembered.is_none() {
                let found =
                    call(backend, "cursor", json!({"target":parsed.target()?}), ctx).await?;
                let terminal = &found["terminal"];
                reachable(terminal)?;
                let mut record = json!({"id":terminal["id"],"env":terminal["env"]});
                if command == "kill-window" {
                    record["window"] = terminal["index"].clone();
                } else {
                    record["offset"] = found["offset"].clone();
                }
                remembered = invocation
                    .memo(memo_key, Some(record.clone()), ctx)
                    .await
                    .map_err(invalid)?
                    .or(Some(record));
            }
            let remembered = remembered.ok_or_else(|| invalid("Missing terminal target"))?;
            (
                json!({"id":remembered["id"],"env":remembered["env"],"index":remembered["window"]}),
                remembered["offset"].clone(),
            )
        };
        reachable(&terminal)?;
        details["terminalId"] = terminal["id"].clone();
        match command {
            "kill-window" => {
                call(
                    backend,
                    "close",
                    json!({"key":key,"target":terminal["id"]}),
                    ctx,
                )
                .await?;
                json!({"window":terminal["index"],"killed":true})
            }
            "capture-pane" => {
                let scrollback = match parsed.options.get(&'S').copied() {
                    None => 0,
                    Some("-") => 100_000,
                    Some(lines) => lines
                        .parse::<i64>()
                        .map_err(invalid)?
                        .saturating_neg()
                        .clamp(0, 100_000),
                };
                let value = call(backend,"capture",json!({"target":terminal["id"],"scrollback":scrollback,"escapes":parsed.options.contains_key(&'e')}),ctx).await?;
                let mut out = state(&value["terminal"], &Value::Null);
                add_output(&mut out, value["text"].as_str().unwrap_or_default(), 0);
                out
            }
            _ => {
                if command == "send-keys" {
                    call(backend,"send",json!({"key":key,"target":terminal["id"],"keys":parsed.rest,"literal":parsed.options.contains_key(&'l')}),ctx).await?;
                }
                let mut request = json!({"target":terminal["id"],"from":offset});
                if !wait.is_null() {
                    request["wait"] = wait;
                }
                let value = call(backend, "read", request, ctx).await?;
                let mut out = state(&value["terminal"], &value["foreground"]);
                if let Some(screen) = value["screen"].as_str() {
                    out["screen"] = json!(true);
                    add_output(&mut out, screen, 0);
                } else {
                    let skipped = value["skipped"].as_u64().unwrap_or_default()
                        + value["truncated"].as_u64().unwrap_or_default();
                    add_output(
                        &mut out,
                        value["text"].as_str().unwrap_or_default(),
                        skipped,
                    );
                }
                if value["waited"] == "timeout" {
                    out["timedOut"] = json!(true);
                }
                out
            }
        }
    };
    if let Some(window) = result.get("window") {
        details["window"] = window.clone();
    }
    Ok(ToolResult {
        output: Some(vec![InputContent::Text(describe(&result).into())]),
        structured_output: Some(result),
        details: Some(details),
        ..Default::default()
    })
}

fn state(terminal: &Value, foreground: &Value) -> Value {
    let mut result = json!({"window":terminal["index"],"running":terminal["exited"].is_null()});
    for (name, value) in [
        ("foreground", foreground),
        ("exitCode", &terminal["exited"]["code"]),
        ("error", &terminal["exited"]["error"]),
    ] {
        if !value.is_null() {
            result[name] = value.clone();
        }
    }
    result
}

fn add_output(out: &mut Value, text: &str, skipped: u64) {
    let (text, omitted) = super::service::tail(text, 16 * 1024);
    out["output"] = json!(text);
    let skipped = skipped + omitted as u64;
    if skipped > 0 {
        out["skipped"] = json!(skipped);
    }
}

fn status(result: &Value) -> String {
    if result["running"] == true {
        return result["foreground"]
            .as_str()
            .map_or_else(|| "at the prompt".into(), |name| format!("running {name}"));
    }
    if let Some(error) = result["error"].as_str() {
        return format!("failed: {error}");
    }
    result["exitCode"].as_i64().map_or_else(
        || "exited".into(),
        |code| format!("exited with code {code}"),
    )
}

fn describe(result: &Value) -> String {
    if let Some(windows) = result["windows"].as_array() {
        if windows.is_empty() {
            return "No windows.".into();
        }
        return windows
            .iter()
            .map(|window| {
                let name = window["name"]
                    .as_str()
                    .or_else(|| window["title"].as_str())
                    .unwrap_or_default();
                let location = window["location"].as_str().unwrap_or_default();
                let opener = format!(
                    "opened by {}",
                    window["opener"].as_str().unwrap_or_default()
                );
                let status = status(window);
                let detail = [name, location, &opener, &status]
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join(" · ");
                format!("{}: {detail}", window["window"])
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    if result["killed"] == true {
        return format!("Killed window {}.", result["window"]);
    }
    let screen = if result["screen"] == true {
        " · full-screen program"
    } else {
        ""
    };
    let mut lines = vec![format!(
        "window {} · {}{screen}",
        result["window"],
        status(result)
    )];
    if let Some(skipped) = result["skipped"].as_u64() {
        lines.push(format!("… {skipped} characters skipped"));
    }
    if let Some(output) = result["output"]
        .as_str()
        .filter(|output| !output.is_empty())
    {
        lines.push(output.to_owned());
    }
    if result["timedOut"] == true && result["running"] == true {
        lines.push(result["foreground"].as_str().map_or_else(
            || "(timed out: still running)".into(),
            |name| format!("(timed out: still running (foreground: {name}))"),
        ));
    }
    lines.join("\n")
}
