//! The sessions plugin on the server. Every session is one row, keyed by its id: what the owner makes,
//! renames, archives and restores; what a session's worker reports (its state); and what the waker
//! starts after a restart. One store owns the rows and offers them as named reads.
//!
//! Upstream keeps the rows in a Chord table of the server's store and serves them as remote objects
//! (`Sessions`, `SessionsList`, `SessionFacts`, `SessionWorker`). `pi-rs-chord` does not yet route
//! those objects by host address, so this port keeps the rows in the Pi's own private document
//! (`sessions.json`) and gives the facades typed Rust entry points; see `MILESTONE.md`.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use indexmap::IndexMap;
use pi_rs_chord::{
    context::Context,
    host::{Facet, PluginFacet, define_facet},
};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::{
    api::v1::contribute::PluginLives, files::write_private, principals::Principal,
    server::SessionRouter,
};

/// Who made a session: the owner, or a person (by id).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Creator {
    Owner,
    Person { id: String },
}

/// What a session's agent is doing: `working`, `waiting`, `settled`, or `failed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    Working,
    Waiting,
    Settled,
    Failed,
}

/// The app every session belongs to until apps exist: `coding`.
pub const CODING: &str = "coding";

/// A session's title: at most 200 characters.
pub const TITLE_MAX: usize = 200;

/// How many sessions a page of a list holds.
pub const PAGE: usize = 50;

/// A session as lists and its worker see it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRow {
    pub id: String,
    pub title: String,
    pub app: String,
    pub creator: Creator,
    pub created_at: u64,
    pub state: SessionState,
    pub last_touch: u64,
    /// `None`: it is not archived.
    pub archived_at: Option<u64>,
}

/// What the sessions store refused.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("sessions: no session {0}")]
    NotFound(String),
    #[error("sessions: a title is at most {TITLE_MAX} characters")]
    TitleTooLong,
    #[error("sessions: the owner's")]
    OwnerOnly,
    #[error("sessions: a session's worker only")]
    WorkerOnly,
    #[error("sessions: the server's waker only")]
    ServerOnly,
    #[error("sessions: {0}")]
    Io(#[from] std::io::Error),
    #[error("sessions: {0}")]
    Corrupt(String),
}

/// The rows on disk: one Pi's sessions.
#[derive(Serialize, Deserialize)]
struct Document {
    sessions: Vec<SessionRow>,
}

struct Inner {
    rows: IndexMap<String, SessionRow>,
    version: u64,
}

/// One Pi's sessions: the rows, their order of change, and the file they live in.
pub struct SessionsStore {
    path: Option<PathBuf>,
    inner: Mutex<Inner>,
    changed: watch::Sender<u64>,
}

impl SessionsStore {
    /// A store in memory: for a test topology, and for a server whose rows are not kept.
    pub fn in_memory() -> Arc<Self> {
        let (changed, _receiver) = watch::channel(0);
        Arc::new(Self {
            path: None,
            inner: Mutex::new(Inner {
                rows: IndexMap::new(),
                version: 0,
            }),
            changed,
        })
    }

    /// The store of the Pi on `path`, its rows read when it was written before.
    pub fn open(path: PathBuf) -> Result<Arc<Self>, SessionError> {
        let mut rows = IndexMap::new();
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let document: Document = serde_json::from_str(&text)
                    .map_err(|error| SessionError::Corrupt(error.to_string()))?;
                for row in document.sessions {
                    rows.insert(row.id.clone(), row);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let (changed, _receiver) = watch::channel(0);
        Ok(Arc::new(Self {
            path: Some(path),
            inner: Mutex::new(Inner { rows, version: 0 }),
            changed,
        }))
    }

    /// A receiver that hears every change of the rows.
    pub fn changed(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    pub fn one(&self, id: &str) -> Option<SessionRow> {
        self.read().rows.get(id).cloned()
    }

    /// An app's sessions that are not archived, the last touched first, then by id.
    pub fn active(&self, app: &str) -> Vec<SessionRow> {
        let mut rows = self
            .read()
            .rows
            .values()
            .filter(|row| row.app == app && row.archived_at.is_none())
            .cloned()
            .collect::<Vec<_>>();
        rows.sort_by(|a, b| {
            b.last_touch
                .cmp(&a.last_touch)
                .then_with(|| a.id.cmp(&b.id))
        });
        rows
    }

    /// An app's archived sessions, the last archived first, then by id.
    pub fn archived(&self, app: &str) -> Vec<SessionRow> {
        let mut rows = self
            .read()
            .rows
            .values()
            .filter(|row| row.app == app && row.archived_at.is_some())
            .cloned()
            .collect::<Vec<_>>();
        rows.sort_by(|a, b| {
            b.archived_at
                .cmp(&a.archived_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        rows
    }

    /// The ids of every session whose agent is working, all of them: whose workers the waker starts.
    pub fn working(&self) -> Vec<String> {
        self.read()
            .rows
            .values()
            .filter(|row| row.state == SessionState::Working)
            .map(|row| row.id.clone())
            .collect()
    }

    /// Makes a session; answers its id.
    pub fn create(&self, title: &str, creator: Creator) -> Result<String, SessionError> {
        if title.chars().count() > TITLE_MAX {
            return Err(SessionError::TitleTooLong);
        }
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_ms();
        let row = SessionRow {
            id: id.clone(),
            title: title.to_string(),
            app: CODING.to_string(),
            creator,
            created_at: now,
            state: SessionState::Settled,
            last_touch: now,
            archived_at: None,
        };
        self.update(|inner| {
            inner.rows.insert(id.clone(), row.clone());
        })?;
        Ok(id)
    }

    pub fn rename(&self, id: &str, title: &str) -> Result<(), SessionError> {
        if title.chars().count() > TITLE_MAX {
            return Err(SessionError::TitleTooLong);
        }
        self.change(id, |row| row.title = title.to_string())
    }

    pub fn archive(&self, id: &str) -> Result<(), SessionError> {
        let now = now_ms();
        self.change(id, |row| row.archived_at = Some(now))
    }

    pub fn restore(&self, id: &str) -> Result<(), SessionError> {
        self.change(id, |row| row.archived_at = None)
    }

    /// Session `id`'s agent is in `state` now: its row says so, and it is touched.
    pub fn report(&self, id: &str, state: SessionState) -> Result<(), SessionError> {
        let now = now_ms();
        self.change(id, |row| {
            row.state = state;
            row.last_touch = now;
        })
    }

    fn change(&self, id: &str, change: impl FnOnce(&mut SessionRow)) -> Result<(), SessionError> {
        self.update(|inner| match inner.rows.get_mut(id) {
            Some(row) => {
                change(row);
                Ok(())
            }
            None => Err(SessionError::NotFound(id.to_string())),
        })?
    }

    fn update<T>(&self, change: impl FnOnce(&mut Inner) -> T) -> Result<T, SessionError> {
        let (result, document) = {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let result = change(&mut inner);
            inner.version += 1;
            let version = inner.version;
            let document = self.path.as_ref().map(|_| Document {
                sessions: inner.rows.values().cloned().collect(),
            });
            (result, (document, version))
        };
        if let (Some(path), Some(document)) = (&self.path, document.0) {
            let text = serde_json::to_string(&document)
                .map_err(|error| SessionError::Corrupt(error.to_string()))?;
            write_private(path, &text)
                .map_err(|error| SessionError::Io(std::io::Error::other(error)))?;
        }
        self.changed.send_replace(document.1);
        Ok(result)
    }

    fn read(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// One screen's list of an app's sessions, active or archived: the rows loaded so far and whether
/// rows may follow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionListValue {
    pub rows: Vec<SessionRow>,
    pub more_after: bool,
}

/// One screen's list of an app's sessions; `more` loads the next page.
pub struct SessionsList {
    store: Arc<SessionsStore>,
    app: String,
    archived: bool,
    loaded: Mutex<usize>,
    page: usize,
}

impl SessionsList {
    pub fn new(store: Arc<SessionsStore>, app: &str, archived: bool, page: usize) -> Self {
        let page = page.max(1);
        Self {
            store,
            app: app.to_string(),
            archived,
            loaded: Mutex::new(page),
            page,
        }
    }

    /// The rows loaded so far, and whether rows may follow.
    pub fn value(&self) -> SessionListValue {
        let all = self.all();
        let loaded = *self
            .loaded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let rows = all.iter().take(loaded).cloned().collect::<Vec<_>>();
        SessionListValue {
            rows,
            more_after: all.len() > loaded,
        }
    }

    /// Loads the next page.
    pub fn more(&self) {
        let mut loaded = self
            .loaded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *loaded = (*loaded + self.page).min(self.all().len());
    }

    fn all(&self) -> Vec<SessionRow> {
        if self.archived {
            self.store.archived(&self.app)
        } else {
            self.store.active(&self.app)
        }
    }
}

/// The owner's and a session's worker's typed entry points to the store: the access checks each
/// upstream object made, before it answered.
pub struct SessionsApi {
    pub store: Arc<SessionsStore>,
}

impl SessionsApi {
    pub fn new(store: Arc<SessionsStore>) -> Self {
        Self { store }
    }

    /// Makes a session for the owner; answers its id.
    pub fn create(
        &self,
        principal: &Principal,
        title: Option<&str>,
    ) -> Result<String, SessionError> {
        self.owner(principal)?;
        self.store.create(title.unwrap_or(""), Creator::Owner)
    }

    pub fn rename(&self, principal: &Principal, id: &str, title: &str) -> Result<(), SessionError> {
        self.owner(principal)?;
        self.store.rename(id, title)
    }

    pub fn archive(&self, principal: &Principal, id: &str) -> Result<(), SessionError> {
        self.owner(principal)?;
        self.store.archive(id)
    }

    pub fn restore(&self, principal: &Principal, id: &str) -> Result<(), SessionError> {
        self.owner(principal)?;
        self.store.restore(id)
    }

    /// One screen's list, for the owner.
    pub fn list(
        &self,
        principal: &Principal,
        app: &str,
        archived: bool,
        page: usize,
    ) -> Result<SessionsList, SessionError> {
        self.owner(principal)?;
        Ok(SessionsList::new(self.store.clone(), app, archived, page))
    }

    /// Session `id`'s row as its own worker sees it: its principal names the session (design section
    /// 4). `None` for a session the server does not know.
    pub fn facts(
        &self,
        principal: &Principal,
        id: &str,
    ) -> Result<Option<SessionRow>, SessionError> {
        if principal.session() != Some(id) {
            return Err(SessionError::WorkerOnly);
        }
        Ok(self.store.one(id))
    }

    /// The worker says what its agent is doing now.
    pub fn report(&self, principal: &Principal, state: SessionState) -> Result<(), SessionError> {
        let Some(id) = principal.session() else {
            return Err(SessionError::WorkerOnly);
        };
        self.store.report(id, state)
    }

    /// A session's row as the owner sees it.
    pub fn worker_row(
        &self,
        principal: &Principal,
        id: &str,
    ) -> Result<Option<SessionRow>, SessionError> {
        self.owner(principal)?;
        Ok(self.store.one(id))
    }

    /// The waker asks the server to start a working session's worker.
    pub fn start(&self, principal: &Principal, id: &str) -> Result<(), SessionError> {
        if principal != &Principal::Server {
            return Err(SessionError::ServerOnly);
        }
        if self.store.one(id).is_none() {
            return Err(SessionError::NotFound(id.to_string()));
        }
        Ok(())
    }

    fn owner(&self, principal: &Principal) -> Result<(), SessionError> {
        if principal.is_owner() {
            Ok(())
        } else {
            Err(SessionError::OwnerOnly)
        }
    }
}

/// Whether `principal` may reach session `id`'s worker: the session must exist, and a worker never
/// reaches another session's worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admission {
    Allow,
    Deny(String),
}

pub fn admit_session(principal: &Principal, exists: bool) -> Admission {
    if principal.session().is_none() && exists {
        Admission::Allow
    } else {
        Admission::Deny("no such session".into())
    }
}

/// The sessions plugin on the server: the waker, once the server is up. A working session was cut off
/// by a restart; its worker starts again, unasked.
pub fn server_facet(
    store: Arc<SessionsStore>,
    router: Arc<SessionRouter>,
    lives: &Arc<PluginLives>,
) -> PluginFacet {
    let lives = lives.clone();
    PluginFacet::new(
        "p3-sessions",
        define_facet(move |_host| -> pi_rs_chord::Result<()> {
            let live = lives.open("p3-sessions");
            let store = store.clone();
            let router = router.clone();
            live.spawn(async move {
                let context = Context::background();
                for id in store.working() {
                    // Held until the worker is live (it is live then), released when the worker start
                    // returns. A worker keeps itself alive while it works.
                    if let Err(error) = router.start(&id, &context).await {
                        eprintln!("p3 sessions: {id}: could not start its worker: {error}");
                    }
                }
            });
            Ok(())
        }),
    )
}

/// What a session's worker may ask the server's sessions plugin. Upstream serves these as the
/// `SessionFacts` and `SessionStart` remote objects; a worker process here has no link to the server
/// yet (the `2.1b-3` seam), so the worker facet takes a link that tests and a future router provide.
pub trait SessionFactsLink: Send + Sync {
    /// The session's row, or `None` for a session the server does not know.
    fn row(&self, session: &str) -> Option<SessionRow>;
    /// Says what the agent is doing now.
    fn report(&self, session: &str, state: SessionState) -> Result<(), SessionError>;
}

/// The sessions plugin on a session's worker: `SessionFacts` as this worker (its principal names the
/// session), and `SessionStart`, which the server's waker holds to start a `working` session's worker
/// after a restart.
pub fn worker_facet(
    facts: Arc<dyn SessionFactsLink>,
    registries: &crate::registry::WorkerRegistries,
    lives: &Arc<PluginLives>,
) -> Facet {
    let registries = registries.clone();
    let lives = lives.clone();
    define_facet(move |_host| -> pi_rs_chord::Result<()> {
        let live = lives.open("p3-sessions");
        live.contribute(&registries.session_facts, facts.clone());
        Ok(())
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Arc<SessionsStore> {
        SessionsStore::in_memory()
    }

    #[test]
    fn a_new_session_starts_settled_in_coding_and_not_archived() {
        let store = store();
        let id = store.create("one", Creator::Owner).expect("create");
        let row = store.one(&id).expect("row");
        assert_eq!(row.title, "one");
        assert_eq!(row.app, CODING);
        assert_eq!(row.state, SessionState::Settled);
        assert_eq!(row.archived_at, None);
        assert!(row.created_at > 0);
    }

    #[test]
    fn active_and_archived_lists_follow_their_order_and_app() {
        let store = store();
        let first = store.create("first", Creator::Owner).expect("create");
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = store.create("second", Creator::Owner).expect("create");
        std::thread::sleep(std::time::Duration::from_millis(2));
        store
            .report(&second, SessionState::Working)
            .expect("report");
        assert_eq!(
            store
                .active(CODING)
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            [second.as_str(), first.as_str()]
        );
        store.archive(&first).expect("archive");
        assert_eq!(
            store
                .active(CODING)
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            [second.as_str()]
        );
        assert_eq!(
            store
                .archived(CODING)
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            [first.as_str()]
        );
        assert!(store.active("other").is_empty());
        store.restore(&first).expect("restore");
        assert!(store.archived(CODING).is_empty());
        assert_eq!(store.active(CODING).len(), 2);
    }

    #[test]
    fn a_rename_of_a_missing_session_is_refused() {
        let store = store();
        assert!(matches!(
            store.rename("missing", "two"),
            Err(SessionError::NotFound(_))
        ));
        let id = store.create("one", Creator::Owner).expect("create");
        store.rename(&id, "two").expect("rename");
        assert_eq!(store.one(&id).expect("row").title, "two");
        let long = "x".repeat(TITLE_MAX + 1);
        assert!(matches!(
            store.rename(&id, &long),
            Err(SessionError::TitleTooLong)
        ));
    }

    #[test]
    fn reports_touch_the_row_and_working_lists_every_working_session() {
        let store = store();
        let id = store.create("one", Creator::Owner).expect("create");
        let before = store.one(&id).expect("row").last_touch;
        store.report(&id, SessionState::Working).expect("report");
        let row = store.one(&id).expect("row");
        assert_eq!(row.state, SessionState::Working);
        assert!(row.last_touch >= before);
        assert_eq!(store.working().as_slice(), std::slice::from_ref(&id));
        store.report(&id, SessionState::Settled).expect("report");
        assert!(store.working().is_empty());
    }

    #[test]
    fn the_owner_alone_reaches_the_stores_writes() {
        let store = store();
        let api = SessionsApi::new(store.clone());
        let owner = Principal::Owner;
        let person = Principal::Person {
            id: "p1".into(),
            name: "Ann".into(),
        };
        let id = api.create(&owner, Some("one")).expect("create");
        assert!(matches!(
            api.create(&person, Some("two")),
            Err(SessionError::OwnerOnly)
        ));
        assert!(matches!(
            api.rename(&person, &id, "two"),
            Err(SessionError::OwnerOnly)
        ));
        assert!(matches!(
            api.worker_row(&person, &id),
            Err(SessionError::OwnerOnly)
        ));
    }

    #[test]
    fn a_worker_reaches_only_its_own_sessions_facts() {
        let store = store();
        let api = SessionsApi::new(store.clone());
        let id = store.create("one", Creator::Owner).expect("create");
        let worker = Principal::worker(&id);
        assert_eq!(
            api.facts(&worker, &id).expect("facts"),
            Some(store.one(&id).expect("row"))
        );
        assert!(matches!(
            api.facts(&Principal::Owner, &id),
            Err(SessionError::WorkerOnly)
        ));
        let other = store.create("two", Creator::Owner).expect("create");
        assert!(matches!(
            api.facts(&worker, &other),
            Err(SessionError::WorkerOnly)
        ));
        api.report(&worker, SessionState::Working).expect("report");
        assert_eq!(store.one(&id).expect("row").state, SessionState::Working);
    }

    #[test]
    fn only_the_server_starts_a_working_sessions_worker() {
        let store = store();
        let api = SessionsApi::new(store.clone());
        let id = store.create("one", Creator::Owner).expect("create");
        assert!(api.start(&Principal::Server, &id).is_ok());
        assert!(matches!(
            api.start(&Principal::Owner, &id),
            Err(SessionError::ServerOnly)
        ));
        assert!(matches!(
            api.start(&Principal::Server, "missing"),
            Err(SessionError::NotFound(_))
        ));
    }

    #[test]
    fn admission_allows_a_session_that_exists_and_never_another_sessions_worker() {
        assert_eq!(admit_session(&Principal::Owner, true), Admission::Allow);
        assert_eq!(
            admit_session(
                &Principal::Person {
                    id: "p1".into(),
                    name: "Ann".into()
                },
                true
            ),
            Admission::Allow
        );
        assert_eq!(
            admit_session(&Principal::worker("s1"), true),
            Admission::Deny("no such session".into())
        );
        assert_eq!(
            admit_session(&Principal::Owner, false),
            Admission::Deny("no such session".into())
        );
    }

    #[test]
    fn a_list_loads_one_page_and_more_loads_the_next() {
        let store = store();
        for index in 0..(PAGE + 5) {
            store
                .create(&format!("{index}"), Creator::Owner)
                .expect("create");
        }
        let list = SessionsList::new(store.clone(), CODING, false, PAGE);
        let value = list.value();
        assert_eq!(value.rows.len(), PAGE);
        assert!(value.more_after);
        list.more();
        let value = list.value();
        assert_eq!(value.rows.len(), PAGE + 5);
        assert!(!value.more_after);
    }

    #[test]
    fn rows_on_disk_come_back() {
        let root = tempfile::tempdir().expect("temp dir");
        let path = root.path().join("sessions.json");
        let id = {
            let store = SessionsStore::open(path.clone()).expect("open");
            let id = store.create("one", Creator::Owner).expect("create");
            store.report(&id, SessionState::Working).expect("report");
            id
        };
        let reopened = SessionsStore::open(path).expect("reopen");
        let row = reopened.one(&id).expect("row");
        assert_eq!(row.title, "one");
        assert_eq!(row.state, SessionState::Working);
        assert_eq!(reopened.working(), [id]);
    }

    #[tokio::test]
    async fn the_waker_starts_the_workers_of_every_working_session() {
        use std::sync::Mutex;

        use crate::{
            api::v1::contribute::PluginLives,
            runner::{RolloutStep, WorkerLink, WorkerRunner},
        };

        struct FakeRunner {
            started: Arc<Mutex<Vec<String>>>,
        }
        impl WorkerRunner for FakeRunner {
            fn start(
                &self,
                session: &str,
                _context: &Context,
            ) -> crate::runner::RunnerFuture<WorkerLink> {
                self.started
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(session.to_string());
                let session = session.to_string();
                Box::pin(async move { Ok(WorkerLink { session, pid: 7 }) })
            }
            fn stop(&self, _session: &str) -> crate::runner::RunnerFuture<()> {
                Box::pin(async { Ok(()) })
            }
            fn retry(&self, _session: &str) {}
            fn state(&self) -> indexmap::IndexMap<String, crate::runner::WorkerState> {
                indexmap::IndexMap::new()
            }
            fn rollout(
                &self,
                _scripts: &[String],
                _context: &Context,
            ) -> crate::runner::RunnerFuture<RolloutStep> {
                Box::pin(async {
                    Ok(RolloutStep {
                        name: "workers".into(),
                        hosts: Vec::new(),
                    })
                })
            }
            fn close(&self) -> crate::runner::RunnerFuture<()> {
                Box::pin(async { Ok(()) })
            }
        }

        let store = store();
        let working = store.create("working", Creator::Owner).expect("create");
        store
            .report(&working, SessionState::Working)
            .expect("report");
        let settled = store.create("settled", Creator::Owner).expect("create");
        let router = SessionRouter::new(store.clone());
        let started = Arc::new(Mutex::new(Vec::new()));
        router.set_runner(Arc::new(FakeRunner {
            started: started.clone(),
        }));
        let lives = PluginLives::new();
        let host = pi_rs_chord::host::create_host(Default::default()).expect("host");
        host.load(
            vec![server_facet(store.clone(), router.clone(), &lives)],
            &Context::background(),
        )
        .await
        .expect("load");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while started
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
            && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let started = started
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(started, [working]);
        assert!(!started.contains(&settled));
        lives.end_all();
        host.close(&Context::background()).await.expect("close");
    }

    #[test]
    fn a_change_is_announced_to_a_watcher() {
        let store = store();
        let mut changed = store.changed();
        let before = *changed.borrow_and_update();
        store.create("one", Creator::Owner).expect("create");
        assert_ne!(*changed.borrow_and_update(), before);
    }
}
