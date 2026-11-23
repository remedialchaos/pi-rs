//! Drive an actual VM while serializing its durable admission and delivery barriers.

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use futures_util::{FutureExt, StreamExt, future::BoxFuture, stream::FuturesUnordered};
use pi_rs_chord::context::{Cancel, Context};
use pi_rs_codemode::{
    CancellationToken, CodemodeResult, DeclaredTool, During, ErrorKind, HostError, RunCall,
    RunController, RunOptions, guest::VmGlobal, source::parse_codemode_source,
};
use serde_json::{Map, Value, json};
use tokio::sync::{Semaphore, mpsc};

use super::{
    CODEMODE_INTERNAL_PREFIX, CODEMODE_MODULE_VERSION, CODEMODE_TOOL_NAME, CodemodeCallStatus,
    CodemodeDeclaredNamespace, CodemodeDeclaredTool, CodemodeDocuments, CodemodeError,
    CodemodeGlobalSpec, CodemodeInvocation, CodemodeJournalEntry, CodemodeJournalGlobal,
    CodemodeJournalState, CodemodeJournalUpdate, CodemodeNestedCall, CodemodeNestedRequest,
    CodemodeNestedResult, CodemodeOutcome, CodemodeSnapshot, CodemodeStart, CodemodeToolDetails,
    CodemodeToolOptions, CodemodeToolResult, DEFAULT_MAX_OUTPUT_TOKENS, GENERATE_IMAGES_TOOL,
    MAX_CONCURRENT_MODEL_CALLS, create_model_tools, declared_namespaces,
    describe::identifier,
    models::{CATALOG_GLOBALS, MODEL_CALLS, catalog_call},
    namespaces_of,
    output::{OutputFiles, byte_end_for_utf16, format_result},
    sample_of,
};
use crate::search::{SearchDocument, rank_documents};

const DISCOVERY_GLOBALS: [&str; 3] = ["searchTools", "describeTool", "describeNamespace"];
const DEFAULT_SEARCH_LIMIT: usize = 8;

pub(super) async fn execute(
    options: &CodemodeToolOptions,
    source: String,
    invocation: CodemodeInvocation<'_>,
    context: &Context,
) -> Result<CodemodeToolResult, CodemodeError> {
    context.check()?;
    let parsed = parse_codemode_source(source).map_err(CodemodeError::Source)?;
    let code = parsed["code"]
        .as_str()
        .ok_or_else(|| CodemodeError::Source("Source parser returned no code".into()))?;
    // The source parser has already checked these integers, including JSON spellings like 3.0 and 3e0.
    let positive_integer = |key| parsed["options"][key].as_f64().map(|value| value as u64);
    let candidate = fresh_start(
        options,
        &invocation,
        context,
        positive_integer("timeoutMs"),
        positive_integer("maxOutputTokens"),
    )
    .await?;
    let (mut start, journal, rerun) = match invocation.documents.begin(candidate, context).await? {
        CodemodeSnapshot::Finished(result) => return Ok(result),
        CodemodeSnapshot::Started {
            start,
            journal,
            rerun,
        } => (start, journal, rerun),
    };
    if start.version != CODEMODE_MODULE_VERSION {
        return decide(invocation.documents, format!("The script cannot resume: it started under code mode version {}, this is {CODEMODE_MODULE_VERSION}.", start.version), CodemodeToolDetails::default(), context).await;
    }
    let remaining = if let Some(deadline) = start.deadline {
        let remaining = deadline.saturating_sub(invocation.tools.now());
        if remaining == 0 {
            return decide(
                invocation.documents,
                "Script timed out: its deadline passed while it was interrupted.".into(),
                CodemodeToolDetails::default(),
                context,
            )
            .await;
        }
        Some(Some(Duration::from_millis(remaining)))
    } else {
        None
    };

    let (events, receiver) = mpsc::unbounded_channel();
    let vm_signal = CancellationToken::default();
    let _abort = context.abort_signal().map(|signal| {
        let vm_signal = vm_signal.clone();
        signal.subscribe(move |reason| vm_signal.cancel(reason))
    });
    let run = options.sandbox.start(
        code,
        RunOptions {
            started_at: start.started_at as f64,
            seed: start.seed,
            tools: start
                .tools
                .iter()
                .filter(|tool| !tool.hidden)
                .map(|tool| DeclaredTool {
                    name: tool.name.clone(),
                    js_name: Some(tool.js_name.clone()),
                    description: tool.description.clone(),
                })
                .collect(),
            globals: start
                .globals
                .iter()
                .map(|global| VmGlobal {
                    name: global.name.clone(),
                    spread: global.spread,
                })
                .collect(),
            store: std::mem::take(&mut start.store),
            timeout: remaining,
            signal: vm_signal,
            on_started: Some(event_callback(&events, VmEvent::Started)),
            on_call: Some(event_callback(&events, VmEvent::Call)),
            on_idle: Some(event_callback(&events, VmEvent::Idle)),
        },
    )?;
    let control = run.controller();
    let (nested_context, cancel) = context.with_cancel();
    let model_slots = Semaphore::new(MAX_CONCURRENT_MODEL_CALLS);
    let driver = Driver {
        options,
        invocation: &invocation,
        context: &nested_context,
        control,
        model_slots: &model_slots,
        start: &start,
    };
    let mut attempt = Attempt::new(&start, journal, rerun)?;
    let (driven, ()) = tokio::join!(
        context.await_with(attempt.drive(&driver, cancel, receiver)),
        async {
            let result = run.promise.await;
            let _ = events.send(VmEvent::Finished(result));
        },
    );
    context.check()?;
    let result = match driven? {
        Ok(result) => result,
        Err(Stop::Refused(message)) => {
            return decide(invocation.documents, message, attempt.details(), context).await;
        }
        Err(Stop::Fatal(error)) => return Err(error),
    };
    if result
        .error()
        .is_some_and(|error| error.kind == ErrorKind::Aborted)
    {
        return Err(CodemodeError::Interrupted);
    }
    let generated_images = attempt.generated_images;
    let mut details = attempt.details();
    let files = OutputFiles::new(invocation.env, invocation.documents, context);
    let output = format_result(
        result,
        generated_images,
        invocation.tools.now().saturating_sub(start.started_at),
        start.max_output_tokens,
        &details.calls,
        &files,
    )
    .await;
    context.check()?;
    details.full_output_path = output.full_output_path;
    invocation
        .documents
        .finish(
            CodemodeToolResult {
                output: output.output,
                is_error: output.is_error,
                details,
            },
            output.writes,
            context,
        )
        .await
}

async fn fresh_start(
    options: &CodemodeToolOptions,
    invocation: &CodemodeInvocation<'_>,
    context: &Context,
    timeout: Option<u64>,
    max_output_tokens: Option<u64>,
) -> Result<CodemodeStart, CodemodeError> {
    let mut catalog = invocation.tools.available_tools(context).await?;
    catalog
        .callable
        .retain(|tool| tool.name != CODEMODE_TOOL_NAME);
    if options.models {
        for tool in create_model_tools() {
            if !catalog
                .callable
                .iter()
                .any(|candidate| candidate.name == tool.name)
            {
                catalog.callable.push(tool);
            }
        }
    }
    let tools: Vec<_> = catalog
        .callable
        .iter()
        .map(|tool| {
            Ok(CodemodeDeclaredTool {
                name: tool.name.clone(),
                js_name: identifier(&tool.name)?,
                description: sample_of(tool)?,
                structured: tool.structured_output_schema.is_some(),
                hidden: tool.name.starts_with(CODEMODE_INTERNAL_PREFIX),
            })
        })
        .collect::<Result<_, CodemodeError>>()?;
    let ownership = namespaces_of(&catalog.extensions);
    let mut by_namespace: HashMap<&str, Vec<String>> = HashMap::new();
    for tool in tools.iter().filter(|tool| !tool.hidden) {
        if let Some(namespace) = ownership.get(tool.name.as_str()) {
            by_namespace
                .entry(&namespace.name)
                .or_default()
                .push(tool.name.clone());
        }
    }
    let namespaces = declared_namespaces(&catalog.extensions)
        .into_iter()
        .map(|namespace| CodemodeDeclaredNamespace {
            name: namespace.name.clone(),
            description: Some(namespace.description.clone()),
            instructions: namespace.instructions.clone(),
            tools: by_namespace
                .remove(namespace.name.as_str())
                .unwrap_or_default(),
        })
        .collect();
    let builtin = DISCOVERY_GLOBALS.into_iter().chain(
        CATALOG_GLOBALS
            .into_iter()
            .chain(MODEL_CALLS.map(|(name, _)| name))
            .filter(|_| options.models),
    );
    let globals = builtin
        .map(|name| CodemodeGlobalSpec {
            name: name.into(),
            spread: true,
        })
        .chain(options.globals.iter().map(|global| CodemodeGlobalSpec {
            name: global.name.clone(),
            spread: global.spread,
        }))
        .collect();
    let now = invocation.tools.now();
    Ok(CodemodeStart {
        version: CODEMODE_MODULE_VERSION,
        started_at: now,
        deadline: timeout.map(|ms| now.saturating_add(ms)),
        seed: rand::random(),
        fingerprint: None,
        tools,
        namespaces,
        globals,
        max_output_tokens: max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
        store: Default::default(),
    })
}

async fn decide(
    documents: &dyn CodemodeDocuments,
    message: String,
    details: CodemodeToolDetails,
    context: &Context,
) -> Result<CodemodeToolResult, CodemodeError> {
    documents
        .finish(
            CodemodeToolResult {
                output: vec![pi_rs_ai::InputContent::Text(message.into())],
                is_error: true,
                details,
            },
            None,
            context,
        )
        .await
}

fn event_callback<T: 'static>(
    events: &mpsc::UnboundedSender<VmEvent>,
    event: fn(T) -> VmEvent,
) -> Arc<dyn Fn(T) + Send + Sync> {
    let events = events.clone();
    Arc::new(move |value| {
        let _ = events.send(event(value));
    })
}

enum VmEvent {
    Started(String),
    Call(RunCall),
    Idle(During),
    Finished(Result<CodemodeResult, HostError>),
}

struct Driver<'a, 'host> {
    options: &'a CodemodeToolOptions,
    invocation: &'a CodemodeInvocation<'host>,
    context: &'a Context,
    control: RunController,
    model_slots: &'a Semaphore,
    start: &'a CodemodeStart,
}

/// Cancel admitted child effects before dropping their waiters, even when this invocation is dropped.
struct StopRun<'a> {
    control: &'a RunController,
    cancel: Cancel,
}

impl Drop for StopRun<'_> {
    fn drop(&mut self) {
        self.cancel.cancel("The script has ended");
        self.control.abort("The script has ended");
    }
}

enum Stop {
    Refused(String),
    Fatal(CodemodeError),
}

impl From<CodemodeError> for Stop {
    fn from(error: CodemodeError) -> Self {
        Self::Fatal(error)
    }
}

fn divergence(message: String) -> Stop {
    Stop::Refused(format!(
        "The script took a different path when resumed after a restart: {message}"
    ))
}

struct Attempt<'a> {
    start: &'a CodemodeStart,
    known: CodemodeJournalState,
    positions: HashMap<u32, usize>,
    rerun: bool,
    admitted_before: u64,
    durable_issued: u64,
    released: bool,
    waiting: VecDeque<RunCall>,
    reattaching: VecDeque<RunCall>,
    ready: HashMap<u32, CodemodeOutcome>,
    last_ack: Option<During>,
    replay_delivered: usize,
    journal_length: u64,
    rows: BTreeMap<u32, CodemodeNestedCall>,
    generated_images: usize,
}

impl<'a> Attempt<'a> {
    fn new(
        start: &'a CodemodeStart,
        known: CodemodeJournalState,
        rerun: bool,
    ) -> Result<Self, CodemodeError> {
        let positions: HashMap<_, _> = known
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.id, index))
            .collect();
        if positions.len() != known.entries.len() {
            return Err(CodemodeError::Documents(
                "A codemode result was journaled more than once".into(),
            ));
        }
        let admitted_before = known
            .entries
            .iter()
            .filter_map(|entry| entry.issued_after)
            .fold(known.first_issued.unwrap_or(0), u64::max);
        Ok(Self {
            start,
            admitted_before,
            durable_issued: admitted_before,
            positions,
            rerun,
            journal_length: known.entries.len() as u64,
            known,
            released: false,
            waiting: VecDeque::new(),
            reattaching: VecDeque::new(),
            ready: HashMap::new(),
            last_ack: None,
            replay_delivered: 0,
            rows: BTreeMap::new(),
            generated_images: 0,
        })
    }

    async fn drive<'work>(
        &mut self,
        driver: &'work Driver<'_, '_>,
        cancel: Cancel,
        mut events: mpsc::UnboundedReceiver<VmEvent>,
    ) -> Result<CodemodeResult, Stop> {
        let mut tasks: Tasks<'work> = FuturesUnordered::new();
        let _stop = StopRun {
            control: &driver.control,
            cancel,
        };
        let mut fingerprinted = false;
        loop {
            tokio::select! {
                biased;
                event = events.recv() => match event {
                    Some(VmEvent::Started(fingerprint)) => {
                        if fingerprinted {
                            return Err(CodemodeError::Dispatch("The sandbox sent a second fingerprint".into()).into());
                        }
                        self.fingerprint(driver, fingerprint).await?;
                        fingerprinted = true;
                    }
                    Some(VmEvent::Call(call)) => self.call(as_tool_call(call))?,
                    Some(VmEvent::Idle(ack)) => {
                        if !driver.control.finished() {
                            if !fingerprinted {
                                return Err(CodemodeError::Dispatch("The sandbox issued calls before sending its fingerprint".into()).into());
                            }
                            self.acknowledge(ack, driver, &mut tasks).await?;
                        }
                    }
                    Some(VmEvent::Finished(result)) => {
                        let result = result.map_err(CodemodeError::from)?;
                        if result.is_ok() && !fingerprinted {
                            return Err(CodemodeError::Dispatch("The sandbox finished without sending its fingerprint".into()).into());
                        }
                        if !result.error().is_some_and(|error| error.kind == ErrorKind::Aborted) {
                            self.check_end(&result)?;
                        }
                        return Ok(result);
                    }
                    None => return Err(CodemodeError::Dispatch("The sandbox event channel closed before the result".into()).into()),
                },
                finished = tasks.next(), if !tasks.is_empty() => {
                    if let Some(finished) = finished && !driver.control.finished() {
                        self.complete_calls(finished, driver, &mut tasks).await?;
                    }
                }
            }
        }
    }

    async fn fingerprint(&self, driver: &Driver<'_, '_>, fingerprint: String) -> Result<(), Stop> {
        if let Some(recorded) = &self.start.fingerprint {
            if recorded != &fingerprint {
                return Err(Stop::Refused(format!(
                    "The script cannot resume: the sandbox changed since it started ({recorded} -> {fingerprint})."
                )));
            }
        } else {
            driver
                .invocation
                .documents
                .record_fingerprint(fingerprint, driver.context)
                .await?;
        }
        Ok(())
    }

    fn call(&mut self, call: RunCall) -> Result<(), Stop> {
        if call.target == "tool" {
            self.rows
                .entry(call.id)
                .or_insert_with(|| CodemodeNestedCall {
                    id: call.id,
                    name: call
                        .name
                        .strip_prefix(CODEMODE_INTERNAL_PREFIX)
                        .unwrap_or(&call.name)
                        .into(),
                    args: preview(
                        call.args.as_ref().map(Value::to_string).unwrap_or_default(),
                        200,
                    ),
                    status: CodemodeCallStatus::Running,
                    duration_ms: None,
                    error: None,
                });
        }
        if let Some(&index) = self.positions.get(&call.id) {
            let entry = &self.known.entries[index];
            if let Some(global) = &entry.global {
                if call.target != "global"
                    || call.name != global.name
                    || !same_optional(&call.args, &global.args)
                {
                    return Err(divergence(format!(
                        "call {} is {}, but was a call to {}",
                        call.id,
                        describe(&call),
                        global.name
                    )));
                }
                self.ready.insert(call.id, global.outcome.clone());
            } else if call.target != "tool" {
                return Err(divergence(format!(
                    "call {} is {}, but was a tool call",
                    call.id,
                    describe(&call)
                )));
            } else {
                self.reattaching.push_back(call);
            }
        } else {
            self.waiting.push_back(call);
        }
        Ok(())
    }

    async fn acknowledge<'work>(
        &mut self,
        ack: During,
        driver: &'work Driver<'_, '_>,
        tasks: &mut Tasks<'work>,
    ) -> Result<(), Stop> {
        let recorded = if ack.seq == 0 {
            self.known.first_issued
        } else {
            self.known
                .entries
                .get((ack.seq - 1) as usize)
                .and_then(|entry| entry.issued_after)
        };
        if let Some(recorded) = recorded {
            if recorded != ack.issued {
                let at = if ack.seq == 0 {
                    "its first run made".into()
                } else {
                    format!("after result {} it had made", ack.seq)
                };
                return Err(divergence(format!(
                    "{at} {} calls, where it had made {recorded}",
                    ack.issued
                )));
            }
        } else {
            driver
                .invocation
                .documents
                .update_journal(
                    CodemodeJournalUpdate {
                        acknowledged: Some(ack),
                        delivered: Vec::new(),
                    },
                    driver.context,
                )
                .await?;
        }
        self.last_ack = Some(ack);
        self.durable_issued = self.durable_issued.max(ack.issued);
        if driver.control.finished() {
            return Ok(());
        }
        if !self.released && ack.seq as usize == self.known.entries.len() {
            // Compare every held key that an earlier attempt might have admitted before releasing any of them.
            if self.rerun {
                for call in self
                    .waiting
                    .iter()
                    .take_while(|call| u64::from(call.id) <= self.admitted_before)
                {
                    check_earlier(driver, call, false).await?;
                }
            }
            self.released = true;
        }
        if let Some(next) = self.known.entries.get(self.replay_delivered)
            && next.id as u64 > ack.issued
        {
            return Err(divergence(format!(
                "it waits without making call {}, which it made before",
                next.id
            )));
        }
        while let Some(call) = take_covered(&mut self.reattaching, ack.issued) {
            check_earlier(driver, &call, true).await?;
            tasks.push(execute_call(driver, call, true).boxed());
        }
        if self.released {
            while let Some(call) = take_covered(&mut self.waiting, self.durable_issued) {
                tasks.push(execute_call(driver, call, false).boxed());
            }
        }
        self.deliver_replayed(&driver.control)
    }

    fn deliver_replayed(&mut self, control: &RunController) -> Result<(), Stop> {
        if self
            .last_ack
            .is_none_or(|ack| ack.seq != self.replay_delivered as u64)
        {
            return Ok(());
        }
        if let Some(entry) = self.known.entries.get(self.replay_delivered)
            && let Some(outcome) = self.ready.remove(&entry.id)
        {
            self.replay_delivered += 1;
            deliver(
                control,
                self.replay_delivered as u64,
                entry.at,
                entry.id,
                outcome,
            )?;
        }
        Ok(())
    }

    async fn complete_calls(
        &mut self,
        first: FinishedCall,
        driver: &Driver<'_, '_>,
        tasks: &mut Tasks<'_>,
    ) -> Result<(), Stop> {
        let mut entries = Vec::new();
        let mut outcomes = Vec::new();
        let mut next = Some(first);
        while let Some(FinishedCall {
            call,
            replay,
            at,
            result,
        }) = next
        {
            if driver.control.finished() {
                break;
            }
            let result = result.map_err(|error| {
                if driver.context.is_aborted() {
                    Stop::Fatal(error)
                } else {
                    divergence(format!("call {} is {}: {error}", call.id, describe(&call)))
                }
            })?;
            let (outcome, global) = match result {
                CallResult::Tool(result) => (self.finish_row(&call, result), None),
                CallResult::Global(outcome) => {
                    let global = CodemodeJournalGlobal {
                        name: call.name,
                        args: call.args,
                        outcome: outcome.clone(),
                    };
                    (outcome, Some(global))
                }
            };
            if replay {
                self.ready.insert(call.id, outcome);
            } else {
                entries.push(CodemodeJournalEntry {
                    id: call.id,
                    at,
                    issued_after: None,
                    global,
                });
                outcomes.push((call.id, at, outcome));
            }
            next = tasks.next().now_or_never().flatten();
        }
        if !entries.is_empty() && !driver.control.finished() {
            driver
                .invocation
                .documents
                .update_journal(
                    CodemodeJournalUpdate {
                        acknowledged: None,
                        delivered: entries,
                    },
                    driver.context,
                )
                .await?;
            for (id, at, outcome) in outcomes {
                self.journal_length += 1;
                deliver(&driver.control, self.journal_length, at, id, outcome)?;
            }
        }
        self.deliver_replayed(&driver.control)
    }

    fn finish_row(&mut self, call: &RunCall, result: CodemodeNestedResult) -> CodemodeOutcome {
        if call.name == GENERATE_IMAGES_TOOL {
            self.generated_images += result
                .structured_output
                .as_ref()
                .and_then(|value| value.get("output"))
                .and_then(Value::as_array)
                .map_or(0, |items| {
                    items
                        .iter()
                        .filter(|item| item.get("type").and_then(Value::as_str) == Some("image"))
                        .count()
                });
        }
        let structured = self
            .start
            .tools
            .iter()
            .any(|tool| tool.name == call.name && tool.structured);
        let (outcome, diagnostic) = outcome_of(
            call,
            result.structured_output,
            structured,
            result.is_error,
            &result.errors,
        );
        if let Some(row) = self.rows.get_mut(&call.id) {
            row.status = if result.is_error {
                CodemodeCallStatus::Error
            } else {
                CodemodeCallStatus::Ok
            };
            row.duration_ms = result.duration_ms;
            row.error = diagnostic.map(|error| preview(error, 500));
        }
        outcome
    }

    fn check_end(&self, result: &CodemodeResult) -> Result<(), Stop> {
        let during = match result {
            CodemodeResult::Success { during, .. } | CodemodeResult::Failure { during, .. } => {
                *during
            }
        };
        if self.rerun
            && let Some(during) = during
            && during.seq as usize <= self.known.entries.len()
        {
            let seq = during.seq as usize;
            let acknowledged = if seq == 0 {
                !self.known.entries.is_empty() || self.known.first_issued.is_some()
            } else {
                self.known.entries[seq - 1].issued_after.is_some()
            };
            let continued = acknowledged
                || self.known.entries[seq..]
                    .iter()
                    .any(|entry| u64::from(entry.id) > during.issued);
            if continued {
                return Err(divergence(format!(
                    "it ended after result {seq}, where it went on before."
                )));
            }
        }
        Ok(())
    }

    fn details(&mut self) -> CodemodeToolDetails {
        let calls = std::mem::take(&mut self.rows)
            .into_values()
            .map(|mut row| {
                if row.status == CodemodeCallStatus::Running {
                    row.status = CodemodeCallStatus::Cancelled;
                }
                row
            })
            .collect();
        CodemodeToolDetails {
            calls,
            full_output_path: None,
        }
    }
}

fn deliver(
    control: &RunController,
    seq: u64,
    at: u64,
    id: u32,
    outcome: CodemodeOutcome,
) -> Result<(), Stop> {
    match control.deliver(seq, at as f64, id, outcome.into()) {
        Ok(()) => Ok(()),
        Err(_) if control.finished() => Ok(()),
        Err(error) => Err(CodemodeError::from(error).into()),
    }
}

fn take_covered(calls: &mut VecDeque<RunCall>, issued: u64) -> Option<RunCall> {
    if calls
        .front()
        .is_some_and(|call| u64::from(call.id) <= issued)
    {
        calls.pop_front()
    } else {
        None
    }
}

async fn check_earlier(
    driver: &Driver<'_, '_>,
    call: &RunCall,
    required: bool,
) -> Result<(), Stop> {
    let key = format!("c{}", call.id);
    let previous = driver
        .invocation
        .tools
        .previous_nested_call(&key, driver.context)
        .await?;
    if let Some(previous) = previous {
        let empty = Value::Object(Map::new());
        if call.target != "tool"
            || call.name != previous.name
            || !same_json(arguments_of(call, &empty), &previous.arguments)
        {
            return Err(divergence(format!(
                "call {} is {}, but was tools.{}({})",
                call.id,
                describe(call),
                previous.name,
                previous.arguments
            )));
        }
    } else if required {
        return Err(divergence(format!(
            "call {} is {}, but its nested task is missing",
            call.id,
            describe(call)
        )));
    }
    Ok(())
}

fn as_tool_call(mut call: RunCall) -> RunCall {
    if call.target == "global"
        && let Some((_, tool)) = MODEL_CALLS.iter().find(|(global, _)| *global == call.name)
    {
        let args = call.args.take().and_then(|args| match args {
            Value::Array(args) => Some(args),
            _ => None,
        });
        let mut args = args.into_iter().flatten();
        let model = args.next().unwrap_or(Value::Null);
        let context = args.next().unwrap_or(Value::Null);
        call.target = "tool".into();
        call.name = (*tool).into();
        call.args = Some(json!({"model": model, "context": context}));
    }
    call
}

fn describe(call: &RunCall) -> String {
    let prefix = if call.target == "tool" { "tools." } else { "" };
    format!(
        "{prefix}{}({})",
        call.name,
        call.args.as_ref().map(Value::to_string).unwrap_or_default()
    )
}

fn preview(mut text: String, max: usize) -> String {
    if byte_end_for_utf16(&text, max) < text.len() {
        text.truncate(byte_end_for_utf16(&text, max - 3));
        text.push_str("...");
    }
    text
}

fn same_optional(a: &Option<Value>, b: &Option<Value>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => same_json(a, b),
        (None, None) => true,
        _ => false,
    }
}

fn same_json(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, val)| b.get(key).is_some_and(|other| same_json(val, other)))
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same_json(a, b))
        }
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        _ => a == b,
    }
}

struct FinishedCall {
    call: RunCall,
    result: Result<CallResult, CodemodeError>,
    replay: bool,
    at: u64,
}

enum CallResult {
    Tool(CodemodeNestedResult),
    Global(CodemodeOutcome),
}

type Tasks<'a> = FuturesUnordered<BoxFuture<'a, FinishedCall>>;

async fn execute_call(driver: &Driver<'_, '_>, call: RunCall, replay: bool) -> FinishedCall {
    let result = if call.target == "tool" {
        execute_nested(driver, &call).await.map(CallResult::Tool)
    } else {
        execute_global(driver, driver.start, &call)
            .await
            .map(CallResult::Global)
    };
    FinishedCall {
        call,
        result,
        replay,
        at: driver.invocation.tools.now(),
    }
}

fn arguments_of<'a>(call: &'a RunCall, default: &'a Value) -> &'a Value {
    call.args
        .as_ref()
        .filter(|value| !value.is_null())
        .unwrap_or(default)
}

async fn execute_nested(
    driver: &Driver<'_, '_>,
    call: &RunCall,
) -> Result<CodemodeNestedResult, CodemodeError> {
    let model_tool = MODEL_CALLS.iter().any(|(_, name)| *name == call.name);
    let _permit = if model_tool {
        Some(
            driver
                .model_slots
                .acquire()
                .await
                .map_err(|error| CodemodeError::Dispatch(error.to_string()))?,
        )
    } else {
        None
    };
    driver.context.check()?;
    if driver.control.finished() {
        return Err(CodemodeError::Dispatch("The script has ended".into()));
    }
    let empty = Value::Object(Map::new());
    let key = format!("c{}", call.id);
    let request = CodemodeNestedRequest {
        key: &key,
        name: &call.name,
        arguments: arguments_of(call, &empty),
        model_tool,
        models: driver.invocation.models,
    };
    driver
        .invocation
        .tools
        .execute_nested(request, driver.context)
        .await
}

async fn execute_global(
    driver: &Driver<'_, '_>,
    start: &CodemodeStart,
    call: &RunCall,
) -> Result<CodemodeOutcome, CodemodeError> {
    driver.context.check()?;
    if driver.control.finished() {
        return Err(CodemodeError::Dispatch("The script has ended".into()));
    }
    let result = if DISCOVERY_GLOBALS.contains(&call.name.as_str()) {
        discover(start, call)
    } else if CATALOG_GLOBALS.contains(&call.name.as_str()) {
        match driver.invocation.models {
            Some(models) => {
                catalog_call(&call.name, call.args.as_ref(), models, driver.context).await
            }
            None => Err(CodemodeError::Model(
                "No model provider is configured".into(),
            )),
        }
    } else if let Some(global) = driver
        .options
        .globals
        .iter()
        .find(|candidate| candidate.name == call.name)
    {
        (global.execute)(call.args.as_ref(), driver.context).await
    } else {
        Err(CodemodeError::Dispatch(format!(
            "Unknown global {}",
            call.name
        )))
    };
    driver.context.check()?;
    Ok(match result {
        Ok(value) => CodemodeOutcome::success(value),
        Err(error) => CodemodeOutcome::failure(error.to_string(), None),
    })
}

fn outcome_of(
    call: &RunCall,
    value: Option<Value>,
    structured: bool,
    is_error: bool,
    errors: &[String],
) -> (CodemodeOutcome, Option<String>) {
    let value = if structured {
        value
    } else {
        let text = match value {
            Some(Value::String(text)) => text,
            Some(Value::Array(blocks)) => blocks
                .iter()
                .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        Some(Value::String(text))
    };
    if !is_error {
        return (CodemodeOutcome::success(value), None);
    }
    if structured && value.is_some() {
        let errors = errors.join("\n");
        return (
            CodemodeOutcome::success(value),
            (!errors.is_empty()).then_some(errors),
        );
    }
    let message = value
        .as_ref()
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map_or_else(
            || {
                let errors = errors.join("\n");
                if errors.is_empty() {
                    format!("Tool {} failed", call.name)
                } else {
                    errors
                }
            },
            str::to_owned,
        );
    let properties = value.map(|value| json!({"structuredOutput": value}));
    (
        CodemodeOutcome::failure(message.clone(), properties),
        Some(message),
    )
}

fn matches_namespace(
    namespace: &CodemodeDeclaredNamespace,
    query: &str,
    normalized: &str,
) -> Result<bool, CodemodeError> {
    Ok(namespace.name == query || identifier(&namespace.name)? == normalized)
}

fn discover(start: &CodemodeStart, call: &RunCall) -> Result<Option<Value>, CodemodeError> {
    let args = call
        .args
        .as_ref()
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let mut tools = start.tools.iter().filter(|tool| !tool.hidden);
    let fail = |message: &str| CodemodeError::Dispatch(message.into());
    let first = args.first().and_then(Value::as_str);
    match call.name.as_str() {
        "describeTool" => {
            let name = first.ok_or_else(|| fail("describeTool() expects a tool name"))?;
            Ok(tools
                .find(|tool| tool.name == name || tool.js_name == name)
                .map(|tool| Value::String(tool.description.clone())))
        }
        "describeNamespace" => {
            let name = first.ok_or_else(|| fail("describeNamespace() expects a namespace name"))?;
            let normalized = identifier(name)?;
            for namespace in &start.namespaces {
                if matches_namespace(namespace, name, &normalized)? {
                    let mut namespace = namespace.clone();
                    namespace.tools = namespace
                        .tools
                        .iter()
                        .map(|tool| identifier(tool))
                        .collect::<Result<_, _>>()?;
                    return Ok(Some(serde_json::to_value(namespace)?));
                }
            }
            Ok(None)
        }
        _ => {
            let query = first.ok_or_else(|| fail("searchTools() expects a query string"))?;
            let options = args.get(1);
            let limit = match options
                .and_then(|value| value.get("limit"))
                .filter(|value| !value.is_null())
            {
                None => DEFAULT_SEARCH_LIMIT,
                Some(value) => value
                    .as_f64()
                    .filter(|limit| limit.is_finite() && limit.fract() == 0.0 && *limit > 0.0)
                    .ok_or_else(|| fail("searchTools() limit must be a positive integer"))?
                    as usize,
            };
            let scope = options
                .and_then(|value| value.get("namespace"))
                .filter(|value| !value.is_null())
                .map(|value| {
                    let name = value
                        .as_str()
                        .ok_or_else(|| fail("searchTools() namespace must be a string"))?;
                    Ok::<_, CodemodeError>((name, identifier(name)?))
                })
                .transpose()?;
            let mut ownership = HashMap::new();
            let namespaces: Vec<_> = start
                .namespaces
                .iter()
                .map(|namespace| {
                    let text = format!(
                        "{} {} {}",
                        namespace.name,
                        namespace.description.as_deref().unwrap_or_default(),
                        namespace.instructions.as_deref().unwrap_or_default()
                    );
                    let include = match &scope {
                        Some((name, normalized)) => matches_namespace(namespace, name, normalized)?,
                        None => true,
                    };
                    Ok((text, include))
                })
                .collect::<Result<_, CodemodeError>>()?;
            for (index, namespace) in start.namespaces.iter().enumerate() {
                for name in &namespace.tools {
                    ownership.entry(name.as_str()).or_insert(index);
                }
            }
            let mut candidates = HashMap::new();
            let documents: Vec<_> = tools
                .filter_map(|tool| {
                    let namespace = ownership
                        .get(tool.name.as_str())
                        .map(|index| &namespaces[*index]);
                    if scope.is_some() && namespace.is_none_or(|(_, include)| !include) {
                        return None;
                    }
                    candidates.insert(tool.name.as_str(), tool);
                    Some(SearchDocument {
                        name: &tool.name,
                        text: &tool.description,
                        namespace_text: namespace.map(|(text, _)| text.as_str()),
                    })
                })
                .collect();
            let result = rank_documents(query, &documents, limit)
                .into_iter()
                .filter_map(|name| candidates.get(name))
                .map(|tool| json!({"name": tool.js_name, "description": tool.description}))
                .collect();
            Ok(Some(Value::Array(result)))
        }
    }
}
