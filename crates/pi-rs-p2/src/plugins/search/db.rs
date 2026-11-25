//! The message index (SQLite FTS5): user/assistant text and catch-up cursors per conversation.

use pi_rs_chord::{
    Result,
    sqlite::{Options, SqliteDatabase, SqliteValue},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

/// Bump when the schema or what is indexed changes: the index is dropped and rebuilt by catch-up.
const SCHEMA_VERSION: i64 = 2;

/// The index's schema: the messages, their cursors and the full-text index over them.
const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS messages (
        rowid INTEGER PRIMARY KEY, session TEXT NOT NULL, conversationId INTEGER NOT NULL,
        entryId INTEGER NOT NULL, role TEXT NOT NULL, timestamp INTEGER NOT NULL, text TEXT NOT NULL,
        UNIQUE(session, conversationId, entryId)
    ) STRICT;
    CREATE TABLE IF NOT EXISTS cursors (
        session TEXT NOT NULL, conversationId INTEGER NOT NULL, through INTEGER NOT NULL,
        PRIMARY KEY(session, conversationId)
    ) STRICT;
    CREATE VIRTUAL TABLE IF NOT EXISTS message_search USING fts5(text, content='messages', content_rowid='rowid');
    CREATE TRIGGER IF NOT EXISTS messages_insert AFTER INSERT ON messages BEGIN
        INSERT INTO message_search(rowid,text) VALUES (new.rowid,new.text);
    END;
    CREATE TRIGGER IF NOT EXISTS messages_delete AFTER DELETE ON messages BEGIN
        INSERT INTO message_search(message_search,rowid,text) VALUES ('delete',old.rowid,old.text);
    END;
";

/// One committed user or assistant message of a session conversation, as indexed.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexedMessage {
    pub conversation_id: i64,
    pub entry_id: i64,
    pub role: String,
    pub timestamp: i64,
    pub text: String,
}

/// A message match, as the search lists it.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageHit {
    pub session: String,
    pub conversation_id: i64,
    pub entry_id: i64,
    pub role: String,
    pub text: String,
}

/// A conversation's own matches, for the history tools.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryHit {
    pub entry_id: i64,
    pub role: String,
    pub snippet: String,
}

/// The messages of one conversation that match, and whether more were found.
#[derive(Clone, Debug, PartialEq)]
pub struct ConversationMessages {
    pub hits: Vec<HistoryHit>,
    pub more: bool,
}

/// The message index of the server's data directory.
#[derive(Clone)]
pub struct SearchDb {
    db: SqliteDatabase,
}

impl SearchDb {
    /// Opens the index, dropping a schema this build does not know (it is derived and rebuildable).
    pub async fn open(path: &Path) -> Result<Self> {
        let db = SqliteDatabase::open(path, Options::default()).await?;
        let version: i64 = db
            .call(|connection| {
                Ok(connection.query_row("PRAGMA user_version", (), |row| row.get(0))?)
            })
            .await?;
        if version != SCHEMA_VERSION {
            db.exec(
                "DROP TRIGGER IF EXISTS messages_insert; DROP TRIGGER IF EXISTS messages_delete;
                 DROP TABLE IF EXISTS message_search; DROP TABLE IF EXISTS messages; DROP TABLE IF EXISTS cursors;",
            )
            .await?;
        }
        db.exec(SCHEMA).await?;
        db.exec(format!("PRAGMA user_version={SCHEMA_VERSION};"))
            .await?;
        // Not derived: what the owner has read survives rebuilds of the index (entry IDs are stable). Answers from before
        // the table existed count as read: unread starts with the next answer, not with every old session.
        let fresh: bool = db
            .call(|connection| {
                Ok(connection
                    .query_row(
                        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='reads'",
                        (),
                        |_| Ok(()),
                    )
                    .is_ok())
            })
            .await?;
        db.exec("CREATE TABLE IF NOT EXISTS reads (session TEXT PRIMARY KEY, through INTEGER NOT NULL) STRICT;")
            .await?;
        if fresh {
            db.exec(
                "INSERT INTO reads(session,through) SELECT session, max(entryId) FROM messages
                 WHERE conversationId=1 AND role='assistant' GROUP BY session",
            )
            .await?;
        }
        Ok(Self { db })
    }

    /// The entry ID up to which the conversation is indexed (0: nothing).
    pub async fn cursor(&self, session: &str, conversation: i64) -> Result<i64> {
        let session = SqliteValue::Text(session.to_owned());
        let conversation = SqliteValue::Integer(conversation);
        self.db
            .call(move |connection| {
                let params: [&SqliteValue; 2] = [&session, &conversation];
                let mut statement = connection
                    .prepare("SELECT through FROM cursors WHERE session=? AND conversationId=?")?;
                let rows = statement.query_map(params, |row| row.get::<_, i64>(0))?;
                let mut through = 0i64;
                for row in rows {
                    through = row?;
                }
                Ok(through)
            })
            .await
    }

    /// Inserts the messages (an entry already indexed is kept) and advances the cursor, atomically.
    pub async fn index(
        &self,
        session: &str,
        conversation: i64,
        messages: &[IndexedMessage],
        through: i64,
    ) -> Result<()> {
        let db = self.db.clone();
        let session = session.to_owned();
        let messages = messages.to_vec();
        db.transaction(move |connection| {
            let mut insert = connection.prepare(
                "INSERT INTO messages (session,conversationId,entryId,role,timestamp,text)
                 VALUES (?,?,?,?,?,?) ON CONFLICT(session,conversationId,entryId) DO NOTHING",
            )?;
            for message in &messages {
                let params: [&SqliteValue; 6] = [
                    &SqliteValue::Text(session.clone()),
                    &SqliteValue::Integer(message.conversation_id),
                    &SqliteValue::Integer(message.entry_id),
                    &SqliteValue::Text(message.role.clone()),
                    &SqliteValue::Integer(message.timestamp),
                    &SqliteValue::Text(message.text.clone()),
                ];
                insert.execute(params)?;
            }
            let params: [&SqliteValue; 3] = [
                &SqliteValue::Text(session.clone()),
                &SqliteValue::Integer(conversation),
                &SqliteValue::Integer(through),
            ];
            connection.execute(
                "INSERT INTO cursors(session,conversationId,through) VALUES (?,?,?)
                 ON CONFLICT(session,conversationId) DO UPDATE SET through=max(through,excluded.through)",
                params,
            )?;
            Ok(())
        })
        .await
    }

    /// The first user message of a session's root conversation, as the session's title.
    pub async fn first_user_text(&self, session: &str) -> Result<Option<String>> {
        let session = SqliteValue::Text(session.to_owned());
        self.db
            .call(move |connection| {
                let params: [&SqliteValue; 1] = [&session];
                let mut statement = connection.prepare(
                    "SELECT text FROM messages WHERE session=? AND conversationId=1 AND role='user'
                     ORDER BY entryId LIMIT 1",
                )?;
                let rows = statement.query_map(params, |row| row.get::<_, String>(0))?;
                let mut text = None;
                for row in rows {
                    text = Some(row?);
                }
                Ok(text)
            })
            .await
    }

    /// The sessions with a message matching the MATCH expression `match` at or after `since`, each with the time of its latest one.
    pub async fn mentions(&self, expression: &str, since: i64) -> Result<BTreeMap<String, i64>> {
        let expression = SqliteValue::Text(expression.to_owned());
        let since = SqliteValue::Integer(since);
        self.db
            .call(move |connection| {
                let params: [&SqliteValue; 2] = [&expression, &since];
                let mut statement = connection.prepare(
                    "SELECT m.session, max(m.timestamp) AS at FROM message_search JOIN messages m ON m.rowid=message_search.rowid
                     WHERE message_search MATCH ? AND m.timestamp >= ? GROUP BY m.session",
                )?;
                let rows = statement.query_map(params, |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?;
                let mut found = BTreeMap::new();
                for row in rows {
                    let (session, at) = row?;
                    found.insert(session, at);
                }
                Ok(found)
            })
            .await
    }

    /// Each session's latest indexed answer: the highest assistant entry ID of its root conversation.
    pub async fn answers(&self) -> Result<BTreeMap<String, i64>> {
        self.db
            .call(|connection| {
                let mut statement = connection.prepare(
                    "SELECT session, max(entryId) AS entryId FROM messages
                     WHERE conversationId=1 AND role='assistant' GROUP BY session",
                )?;
                let rows = statement.query_map((), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?;
                let mut answers = BTreeMap::new();
                for row in rows {
                    let (session, entry) = row?;
                    answers.insert(session, entry);
                }
                Ok(answers)
            })
            .await
    }

    /// Up to which root entry the owner has read each session.
    pub async fn reads(&self) -> Result<BTreeMap<String, i64>> {
        self.db
            .call(|connection| {
                let mut statement = connection.prepare("SELECT session, through FROM reads")?;
                let rows = statement.query_map((), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?;
                let mut reads = BTreeMap::new();
                for row in rows {
                    let (session, through) = row?;
                    reads.insert(session, through);
                }
                Ok(reads)
            })
            .await
    }

    /// Records that the owner has read `session` through `through` (never moves back).
    pub async fn mark_read(&self, session: &str, through: i64) -> Result<()> {
        let db = self.db.clone();
        let session = session.to_owned();
        db.transaction(move |connection| {
            let params: [&SqliteValue; 2] = [
                &SqliteValue::Text(session.clone()),
                &SqliteValue::Integer(through),
            ];
            connection.execute(
                "INSERT INTO reads(session,through) VALUES (?,?) ON CONFLICT(session) DO UPDATE
                 SET through=max(through,excluded.through)",
                params,
            )?;
            Ok(())
        })
        .await
    }

    /// Drops a session's messages and cursor (its worker re-indexes from the start).
    pub async fn reset(&self, session: &str) -> Result<()> {
        let db = self.db.clone();
        let session = session.to_owned();
        db.transaction(move |connection| {
            let params: [&SqliteValue; 1] = [&SqliteValue::Text(session.clone())];
            connection.execute("DELETE FROM messages WHERE session=?", params)?;
            connection.execute("DELETE FROM cursors WHERE session=?", params)?;
            Ok(())
        })
        .await
    }

    /// Best matches of every literal term in `sessions`; bounded snippets, at most 41.
    pub async fn messages(&self, expression: &str, sessions: &[String]) -> Result<Vec<MessageHit>> {
        if sessions.is_empty() {
            return Ok(Vec::new());
        }
        let expression = SqliteValue::Text(expression.to_owned());
        let listed =
            SqliteValue::Text(serde_json::to_string(sessions).map_err(pi_rs_chord::Error::Json)?);
        self.db
            .call(move |connection| {
                let params: [&SqliteValue; 2] = [&expression, &listed];
                let mut statement = connection.prepare(
                    "SELECT m.session,m.conversationId,m.entryId,m.role,
                     substr(snippet(message_search,0,'','','…',24),1,300) AS text
                     FROM message_search JOIN messages m ON m.rowid=message_search.rowid
                     WHERE message_search MATCH ? AND m.session IN (SELECT value FROM json_each(?))
                     ORDER BY rank,m.entryId DESC LIMIT 41",
                )?;
                let rows = statement.query_map(params, |row| {
                    Ok(MessageHit {
                        session: row.get::<_, String>(0)?,
                        conversation_id: row.get::<_, i64>(1)?,
                        entry_id: row.get::<_, i64>(2)?,
                        role: row.get::<_, String>(3)?,
                        text: row.get::<_, String>(4)?,
                    })
                })?;
                let mut hits = Vec::new();
                for row in rows {
                    hits.push(row?);
                }
                Ok(hits)
            })
            .await
    }

    /// Matches of the MATCH expression `match` in one conversation, below entry `before`.
    pub async fn conversation_messages(
        &self,
        session: &str,
        conversation: i64,
        expression: &str,
        before: Option<i64>,
        limit: i64,
    ) -> Result<ConversationMessages> {
        let session = SqliteValue::Text(session.to_owned());
        let conversation = SqliteValue::Integer(conversation);
        let expression = SqliteValue::Text(expression.to_owned());
        let before = SqliteValue::Integer(before.unwrap_or(i64::MAX));
        let bounded = limit + 1;
        let bound = SqliteValue::Integer(bounded);
        self.db
            .call(move |connection| {
                let params: [&SqliteValue; 5] =
                    [&expression, &session, &conversation, &before, &bound];
                let mut statement = connection.prepare(
                    "SELECT m.entryId,m.role,substr(snippet(message_search,0,'','','…',24),1,300) AS snippet
                     FROM message_search JOIN messages m ON m.rowid=message_search.rowid
                     WHERE message_search MATCH ? AND m.session=? AND m.conversationId=? AND m.entryId<?
                     ORDER BY rank,m.entryId DESC LIMIT ?",
                )?;
                let rows = statement.query_map(params, |row| {
                    Ok(HistoryHit {
                        entry_id: row.get::<_, i64>(0)?,
                        role: row.get::<_, String>(1)?,
                        snippet: row.get::<_, String>(2)?,
                    })
                })?;
                let mut hits = Vec::new();
                for row in rows {
                    hits.push(row?);
                }
                let more = hits.len() as i64 > limit;
                hits.truncate(limit as usize);
                Ok(ConversationMessages { hits, more })
            })
            .await
    }

    /// Closes the index.
    pub async fn close(&self) -> Result<()> {
        self.db.close().await
    }
}
