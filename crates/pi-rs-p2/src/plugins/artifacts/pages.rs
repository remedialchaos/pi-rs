//! Open html pages of each artifact (server, in memory): errors, logs, carried cursors, and
//! eval requests dispatched to open browser pages.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use pi_rs_chord::context::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;
use uuid::Uuid;

/// How many errors are kept per artifact (the newest).
const KEPT_ERRORS: usize = 50;
/// How many log lines are kept per artifact (the newest).
const KEPT_LOGS: usize = 500;
/// How long the agent's code may run in a page before the request fails.
const EVAL_TIMEOUT: Duration = Duration::from_secs(30);

/// What an html page logged: its level and text, numbered in arrival order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    pub seq: i64,
    pub level: String,
    pub text: String,
    pub at: i64,
}

/// The result of code an open page ran: its JSON value, or the error it threw.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EvalResult {
    Ok { ok: bool, value: Value },
    Err { ok: bool, error: String },
}

impl EvalResult {
    pub fn ok(value: Value) -> Self {
        Self::Ok { ok: true, value }
    }

    pub fn err(error: impl Into<String>) -> Self {
        Self::Err {
            ok: false,
            error: error.into(),
        }
    }
}

/// An uncarried error reported by an open page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UncarriedError {
    pub id: String,
    pub text: String,
}

/// A cursor marking carried errors up through `seq`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorMark {
    pub id: String,
    pub seq: i64,
}

/// A request emitted to an open html page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalRequest {
    pub request: String,
    pub code: String,
}

/// Callback that pushes an eval request to an open page.
pub type PageRunner = Arc<dyn Fn(EvalRequest) + Send + Sync>;

struct RegisteredPage {
    token: u64,
    run: PageRunner,
}

#[derive(Default)]
struct Reported {
    errors: Vec<LogEntry>,
    logs: Vec<LogEntry>,
    carried: i64,
    seq: i64,
}

#[derive(Default)]
struct Inner {
    pages: HashMap<String, Vec<RegisteredPage>>,
    reports: HashMap<String, Reported>,
    pending: HashMap<String, oneshot::Sender<EvalResult>>,
}

/// The open pages and their reports, keyed by `<session>/<id>`.
pub struct PageRegistry {
    inner: Mutex<Inner>,
    next_token: AtomicU64,
    eval_timeout: Duration,
}

impl Default for PageRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Unregisters an open page when dropped.
pub struct PageGuard {
    registry: Arc<PageRegistry>,
    key: String,
    token: u64,
}

impl Drop for PageGuard {
    fn drop(&mut self) {
        let mut inner = self
            .registry
            .inner
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if let Some(pages) = inner.pages.get_mut(&self.key) {
            pages.retain(|entry| entry.token != self.token);
            if pages.is_empty() {
                inner.pages.remove(&self.key);
            }
        }
    }
}

impl PageRegistry {
    pub fn new() -> Self {
        Self::with_timeout(EVAL_TIMEOUT)
    }

    pub(crate) fn with_timeout(eval_timeout: Duration) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            next_token: AtomicU64::new(1),
            eval_timeout,
        }
    }

    /// Registers an open page of artifact `id` of `session`; dropping the guard unregisters it.
    pub fn open(self: &Arc<Self>, session: &str, id: &str, run: PageRunner) -> PageGuard {
        let key = key_of(session, id);
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        inner
            .pages
            .entry(key.clone())
            .or_default()
            .push(RegisteredPage { token, run });
        PageGuard {
            registry: Arc::clone(self),
            key,
            token,
        }
    }

    /// Records new errors and log lines reported by a page.
    pub fn report(&self, session: &str, id: &str, errors: &[String], logs: &[(String, String)]) {
        let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        let reported = inner.reports.entry(key_of(session, id)).or_default();
        let at = now_ms();
        for text in errors {
            reported.seq += 1;
            reported.errors.push(LogEntry {
                seq: reported.seq,
                level: "error".into(),
                text: text.clone(),
                at,
            });
        }
        for (level, text) in logs {
            reported.seq += 1;
            reported.logs.push(LogEntry {
                seq: reported.seq,
                level: level.clone(),
                text: text.clone(),
                at,
            });
        }
        if reported.errors.len() > KEPT_ERRORS {
            let drain = reported.errors.len() - KEPT_ERRORS;
            reported.errors.drain(0..drain);
        }
        if reported.logs.len() > KEPT_LOGS {
            let drain = reported.logs.len() - KEPT_LOGS;
            reported.logs.drain(0..drain);
        }
    }

    /// The errors kept for artifact `id`, oldest first.
    pub fn errors(&self, session: &str, id: &str) -> Vec<LogEntry> {
        let inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        inner
            .reports
            .get(&key_of(session, id))
            .map(|reported| reported.errors.clone())
            .unwrap_or_default()
    }

    /// The log lines kept after `since`, and the newest sequence number.
    pub fn logs(&self, session: &str, id: &str, since: i64) -> (Vec<LogEntry>, i64) {
        let inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        match inner.reports.get(&key_of(session, id)) {
            Some(reported) => (
                reported
                    .logs
                    .iter()
                    .filter(|entry| entry.seq > since)
                    .cloned()
                    .collect(),
                reported.seq,
            ),
            None => (Vec::new(), since),
        }
    }

    /// Errors of `session`'s artifacts not yet carried by a message, plus marks for `carried`.
    pub fn uncarried(&self, session: &str) -> (Vec<UncarriedError>, Vec<ErrorMark>) {
        let inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        let prefix = format!("{session}/");
        let mut errors = Vec::new();
        let mut marks = Vec::new();
        for (key, reported) in &inner.reports {
            let Some(id) = key.strip_prefix(&prefix) else {
                continue;
            };
            let fresh: Vec<&LogEntry> = reported
                .errors
                .iter()
                .filter(|entry| entry.seq > reported.carried)
                .collect();
            if let Some(last) = fresh.last() {
                for entry in &fresh {
                    errors.push(UncarriedError {
                        id: id.to_owned(),
                        text: entry.text.clone(),
                    });
                }
                marks.push(ErrorMark {
                    id: id.to_owned(),
                    seq: last.seq,
                });
            }
        }
        (errors, marks)
    }

    /// Marks errors up to each mark's sequence number as carried.
    pub fn carried(&self, session: &str, marks: &[ErrorMark]) {
        let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        for mark in marks {
            if let Some(reported) = inner.reports.get_mut(&key_of(session, &mark.id))
                && mark.seq > reported.carried
            {
                reported.carried = mark.seq;
            }
        }
    }

    /// Runs `code` in the first open page of artifact `id`.
    pub async fn run(&self, session: &str, id: &str, code: &str, context: &Context) -> EvalResult {
        let key = key_of(session, id);
        let request = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        let runner = {
            let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
            let Some(runner) = inner
                .pages
                .get(&key)
                .and_then(|pages| pages.first())
                .map(|first| Arc::clone(&first.run))
            else {
                return EvalResult::err(format!(
                    "Artifact \"{id}\" is not open on any device: artifact_open or artifact_show it first"
                ));
            };
            inner.pending.insert(request.clone(), tx);
            runner
        };
        runner(EvalRequest {
            request: request.clone(),
            code: code.to_owned(),
        });
        let timeout = self.eval_timeout;
        let waited = context.await_with(tokio::time::timeout(timeout, rx)).await;
        let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        inner.pending.remove(&request);
        match waited {
            Err(_) => EvalResult::err("Cancelled"),
            Ok(Err(_)) => EvalResult::err(format!(
                "The page did not answer within {} s",
                timeout.as_secs()
            )),
            Ok(Ok(Ok(result))) => result,
            Ok(Ok(Err(_))) => EvalResult::err("Cancelled"),
        }
    }

    /// Delivers a page's answer to `request` (first answer wins; late or unknown ignored).
    pub fn answer(&self, request: &str, result: EvalResult) {
        let sender = {
            let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
            inner.pending.remove(request)
        };
        if let Some(sender) = sender {
            let _ = sender.send(result);
        }
    }

    /// Forgets what artifact `id` reported (it was removed).
    pub fn forget(&self, session: &str, id: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        inner.reports.remove(&key_of(session, id));
    }
}

fn key_of(session: &str, id: &str) -> String {
    format!("{session}/{id}")
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
