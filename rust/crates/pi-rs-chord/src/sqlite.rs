//! A FIFO SQLite connection actor. No SQL executes on Tokio's worker threads.
use crate::{Error, Result};
use rusqlite::{Connection, OpenFlags};
use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::mpsc,
    time::Duration,
};
use tokio::sync::oneshot;

pub use rusqlite::types::Value as SqliteValue;
pub type SqliteFuture<T> = Pin<Box<dyn Future<Output = Result<T>> + Send>>;
type Operation = Box<dyn FnOnce(&mut Option<Connection>) + Send>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    ReadWrite,
    ReadOnly,
}
#[derive(Clone, Debug)]
pub struct Options {
    pub mode: Mode,
    pub busy_timeout_ms: u64,
    pub wal_auto_checkpoint_pages: u32,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            mode: Mode::ReadWrite,
            busy_timeout_ms: 5_000,
            wal_auto_checkpoint_pages: 1_000,
        }
    }
}
#[derive(Clone)]
pub struct SqliteDatabase {
    sender: mpsc::Sender<Operation>,
    mode: Mode,
}

impl SqliteDatabase {
    pub async fn open(path: impl AsRef<Path>, options: Options) -> Result<Self> {
        let path = PathBuf::from(path.as_ref());
        let mode = options.mode;
        let (sender, receiver) = mpsc::channel::<Operation>();
        let (ready, opened) = oneshot::channel();
        std::thread::Builder::new()
            .name("chord-sqlite".into())
            .spawn(move || {
                let connection = (|| -> Result<Connection> {
                    if mode == Mode::ReadWrite
                        && path != Path::new(":memory:")
                        && let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
                    {
                        std::fs::create_dir_all(parent)?;
                    }
                    let flags = match mode {
                        Mode::ReadOnly => OpenFlags::SQLITE_OPEN_READ_ONLY,
                        Mode::ReadWrite => {
                            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
                        }
                    } | OpenFlags::SQLITE_OPEN_NO_MUTEX;
                    let connection = Connection::open_with_flags(path, flags)?;
                    connection.busy_timeout(Duration::from_millis(options.busy_timeout_ms))?;
                    connection.set_prepared_statement_cache_capacity(256);
                    if mode == Mode::ReadWrite {
                        connection.pragma_update(None, "journal_mode", "WAL")?;
                        connection.pragma_update(None, "synchronous", "NORMAL")?;
                        connection.pragma_update(
                            None,
                            "wal_autocheckpoint",
                            options.wal_auto_checkpoint_pages,
                        )?;
                    }
                    Ok(connection)
                })();
                match connection {
                    Err(error) => {
                        let _ = ready.send(Err(error));
                    }
                    Ok(connection) => {
                        let mut connection = Some(connection);
                        if ready.send(Ok(())).is_err() {
                            return;
                        }
                        for operation in receiver {
                            operation(&mut connection);
                        }
                        if let Some(connection) = connection {
                            if mode == Mode::ReadWrite {
                                let _ = connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
                            }
                            let _ = connection.close();
                        }
                    }
                }
            })?;
        opened.await.map_err(|_| Error::Closed)??;
        Ok(Self { sender, mode })
    }

    /// Enqueues immediately, before the returned future is polled.
    pub fn call<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&Connection) -> Result<T> + Send + 'static,
    ) -> SqliteFuture<T> {
        let (reply, result) = oneshot::channel();
        let queued = self.sender.send(Box::new(move |connection| {
            let value = match connection {
                Some(connection) => operation(connection),
                None => Err(Error::Closed),
            };
            let _ = reply.send(value);
        }));
        Box::pin(async move {
            queued.map_err(|_| Error::Closed)?;
            result.await.map_err(|_| Error::Closed)?
        })
    }

    /// The borrowed connection cannot escape its transaction scope.
    pub fn transaction<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&Connection) -> Result<T> + Send + 'static,
    ) -> SqliteFuture<T> {
        let begin = if self.mode == Mode::ReadOnly {
            "BEGIN"
        } else {
            "BEGIN IMMEDIATE"
        };
        self.call(move |connection| {
            connection.execute_batch(begin)?;
            let result = operation(connection).and_then(|value| {
                connection.execute_batch("COMMIT")?;
                Ok(value)
            });
            match result {
                Ok(value) => Ok(value),
                Err(operation) => match connection.execute_batch("ROLLBACK") {
                    Ok(()) => Err(operation),
                    Err(rollback) => Err(Error::Rollback {
                        operation: Box::new(operation),
                        rollback,
                    }),
                },
            }
        })
    }

    pub fn exec(&self, sql: impl Into<String>) -> SqliteFuture<()> {
        let sql = sql.into();
        self.call(move |c| {
            c.execute_batch(&sql)?;
            Ok(())
        })
    }
    pub fn run(&self, sql: impl Into<String>, params: Vec<SqliteValue>) -> SqliteFuture<usize> {
        let sql = sql.into();
        self.call(move |c| {
            Ok(c.prepare_cached(&sql)?
                .execute(rusqlite::params_from_iter(params))?)
        })
    }
    pub fn close(&self) -> SqliteFuture<()> {
        let mode = self.mode;
        let (reply, result) = oneshot::channel();
        let queued = self.sender.send(Box::new(move |connection| {
            let result = if let Some(connection) = connection.take() {
                connection.flush_prepared_statement_cache();
                let checkpoint = if mode == Mode::ReadWrite {
                    connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                } else {
                    Ok(())
                };
                let close = connection.close().map_err(|(_, e)| Error::Sqlite(e));
                checkpoint.map_err(Error::from).and(close)
            } else {
                Ok(())
            };
            let _ = reply.send(result);
        }));
        Box::pin(async move {
            queued.map_err(|_| Error::Closed)?;
            result.await.map_err(|_| Error::Closed)?
        })
    }
}

#[derive(Clone, Debug)]
pub struct Migration {
    pub version: u32,
    pub statements: Vec<String>,
}

pub fn validate_sqlite_schema_prefix(prefix: &str) -> Result<()> {
    let mut chars = prefix.chars();
    if !chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(Error::Invalid(format!(
            "Invalid SQLite schema prefix {}",
            serde_json::to_string(prefix)?
        )));
    }
    if prefix.to_ascii_lowercase().starts_with("sqlite_") {
        return Err(Error::Invalid(format!(
            "SQLite schema prefix {} is reserved",
            serde_json::to_string(prefix)?
        )));
    }
    Ok(())
}

pub fn apply_migrations_in(
    connection: &Connection,
    table: &str,
    migrations: &[Migration],
) -> Result<()> {
    validate_sqlite_schema_prefix(table)?;
    if migrations
        .iter()
        .enumerate()
        .any(|(i, m)| m.version as usize != i + 1)
    {
        return Err(Error::Invalid(
            "SQLite migrations must have contiguous versions starting at 1".into(),
        ));
    }
    connection.execute_batch(&format!("CREATE TABLE IF NOT EXISTS {table} (singleton INTEGER PRIMARY KEY CHECK (singleton = 1), version INTEGER NOT NULL CHECK (version >= 0)) STRICT; INSERT OR IGNORE INTO {table} (singleton, version) VALUES (1, 0)"))?;
    let version: u32 = connection.query_row(
        &format!("SELECT version FROM {table} WHERE singleton = 1"),
        [],
        |r| r.get(0),
    )?;
    let supported = migrations.last().map_or(0, |m| m.version);
    if version > supported {
        return Err(Error::Invalid(format!(
            "SQLite schema version {version} is newer than supported version {supported}"
        )));
    }
    for migration in migrations.iter().filter(|m| m.version > version) {
        for statement in &migration.statements {
            connection.execute_batch(statement)?;
        }
        connection.execute(
            &format!("UPDATE {table} SET version = ? WHERE singleton = 1"),
            [migration.version],
        )?;
    }
    Ok(())
}

pub fn apply_sqlite_migrations(
    database: &SqliteDatabase,
    table: String,
    migrations: Vec<Migration>,
) -> SqliteFuture<()> {
    // Validate versions inside the same closure, before touching SQLite.
    database.transaction(move |connection| apply_migrations_in(connection, &table, &migrations))
}
