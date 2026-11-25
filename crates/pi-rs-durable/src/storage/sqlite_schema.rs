use pi_rs_chord::sqlite::Migration;

pub const CURRENT_SQLITE_SCHEMA_VERSION: u32 = 2;

pub(super) fn migrations(prefix: &str) -> Vec<Migration> {
    vec![
        Migration {
            version: 1,
            statements: vec![
                format!(
                    r#"CREATE TABLE {prefix}durable_metadata (
                singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                next_id TEXT NOT NULL,
                next_seq INTEGER NOT NULL
		) STRICT"#
                ),
                format!(
                    r#"INSERT INTO {prefix}durable_metadata (singleton, next_id, next_seq) VALUES (1, '2', 1)"#
                ),
                format!(
                    r#"CREATE TABLE {prefix}record_ids (
                id INTEGER PRIMARY KEY,
                record_type TEXT NOT NULL CHECK (record_type IN ('conversation', 'entry', 'task', 'submission', 'document'))
		) STRICT"#
                ),
                format!(
                    r#"CREATE TABLE {prefix}conversations (
                id INTEGER PRIMARY KEY,
                owner_conversation_id INTEGER,
                owner_task_id INTEGER,
                record TEXT NOT NULL CHECK (json_valid(record))
		) STRICT"#
                ),
                format!(
                    r#"CREATE INDEX {prefix}conversations_by_owner_conversation ON {prefix}conversations (owner_conversation_id, id)"#
                ),
                format!(
                    r#"CREATE INDEX {prefix}conversations_by_owner_task ON {prefix}conversations (owner_task_id, id)"#
                ),
                format!(
                    r#"CREATE TABLE {prefix}entries (
                id INTEGER PRIMARY KEY,
                conversation_id INTEGER NOT NULL,
                head INTEGER,
                commit_seq INTEGER NOT NULL,
                record TEXT NOT NULL CHECK (json_valid(record))
		) STRICT"#
                ),
                format!(
                    r#"CREATE INDEX {prefix}entries_by_conversation ON {prefix}entries (conversation_id, id DESC)"#
                ),
                format!(
                    r#"CREATE INDEX {prefix}entry_heads_by_conversation ON {prefix}entries (conversation_id, id DESC) WHERE head IS NOT NULL"#
                ),
                format!(
                    r#"CREATE TABLE {prefix}tasks (
                id INTEGER PRIMARY KEY,
                conversation_id INTEGER NOT NULL,
                kind TEXT NOT NULL,
                status TEXT NOT NULL CHECK (status IN ('pending', 'running', 'waiting', 'completing', 'terminal')),
                abort_requested INTEGER NOT NULL CHECK (abort_requested IN (0, 1)),
                background INTEGER NOT NULL CHECK (background IN (0, 1)),
                record TEXT NOT NULL CHECK (json_valid(record))
		) STRICT"#
                ),
                format!(r#"CREATE INDEX {prefix}tasks_by_status ON {prefix}tasks (status, id)"#),
                format!(
                    r#"CREATE INDEX {prefix}tasks_by_conversation ON {prefix}tasks (conversation_id, id)"#
                ),
                format!(r#"CREATE INDEX {prefix}tasks_by_kind ON {prefix}tasks (kind, id)"#),
                format!(
                    r#"CREATE INDEX {prefix}tasks_by_abort_requested ON {prefix}tasks (abort_requested, id)"#
                ),
                format!(
                    r#"CREATE INDEX {prefix}tasks_by_background ON {prefix}tasks (background, id)"#
                ),
                format!(
                    r#"CREATE TABLE {prefix}submissions (
                id INTEGER PRIMARY KEY,
                conversation_id INTEGER NOT NULL,
                request_id TEXT,
                status TEXT NOT NULL CHECK (status IN ('queued', 'placed', 'done', 'unanswered')),
                record TEXT NOT NULL CHECK (json_valid(record))
		) STRICT"#
                ),
                format!(
                    r#"CREATE INDEX {prefix}submissions_by_request ON {prefix}submissions (conversation_id, request_id)"#
                ),
                format!(
                    r#"CREATE INDEX {prefix}submissions_by_conversation ON {prefix}submissions (conversation_id, id)"#
                ),
                format!(
                    r#"CREATE INDEX {prefix}submissions_by_status ON {prefix}submissions (status, id)"#
                ),
                format!(
                    r#"CREATE TABLE {prefix}documents (
                id INTEGER PRIMARY KEY,
                kind TEXT NOT NULL,
                family INTEGER NOT NULL CHECK (family IN (0, 1)),
                key_value TEXT NOT NULL,
                scope_kind TEXT NOT NULL CHECK (scope_kind IN ('session', 'conversation', 'task')),
                owner_id INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                retired_at INTEGER,
                record TEXT NOT NULL CHECK (json_valid(record))
		) STRICT"#
                ),
                format!(
                    r#"CREATE INDEX {prefix}documents_by_address
                ON {prefix}documents (kind, scope_kind, owner_id, family, key_value, created_at DESC, retired_at)"#
                ),
                format!(
                    r#"CREATE INDEX {prefix}documents_by_scope ON {prefix}documents (scope_kind, owner_id, id)"#
                ),
                format!(
                    r#"CREATE INDEX {prefix}documents_by_scope_kind ON {prefix}documents (scope_kind, owner_id, kind, id)"#
                ),
                format!(
                    r#"CREATE TABLE {prefix}document_revisions (
                document_id INTEGER NOT NULL,
                seq INTEGER NOT NULL,
                kind TEXT NOT NULL CHECK (kind IN ('base', 'delta')),
                version INTEGER NOT NULL,
                content TEXT NOT NULL CHECK (json_valid(content)),
                PRIMARY KEY (document_id, seq)
		) STRICT"#
                ),
                format!(
                    r#"CREATE INDEX {prefix}document_revisions_by_kind ON {prefix}document_revisions (document_id, kind, seq DESC)"#
                ),
            ],
        },
        Migration {
            version: 2,
            statements: vec![
                format!(
                    r#"CREATE TABLE {prefix}tasks_v2 (
                id INTEGER PRIMARY KEY,
                conversation_id INTEGER NOT NULL,
                kind TEXT NOT NULL,
                status TEXT NOT NULL CHECK (status IN ('pending', 'running', 'waiting', 'sleeping', 'completing', 'terminal')),
                abort_requested INTEGER NOT NULL CHECK (abort_requested IN (0, 1)),
                background INTEGER NOT NULL CHECK (background IN (0, 1)),
                record TEXT NOT NULL CHECK (json_valid(record))
		) STRICT"#
                ),
                format!(
                    r#"INSERT INTO {prefix}tasks_v2 (id, conversation_id, kind, status, abort_requested, background, record)
                SELECT id, conversation_id, kind, status, abort_requested, background, record FROM {prefix}tasks"#
                ),
                format!(r#"DROP TABLE {prefix}tasks"#),
                format!(r#"ALTER TABLE {prefix}tasks_v2 RENAME TO {prefix}tasks"#),
                format!(r#"CREATE INDEX {prefix}tasks_by_status ON {prefix}tasks (status, id)"#),
                format!(
                    r#"CREATE INDEX {prefix}tasks_by_conversation ON {prefix}tasks (conversation_id, id)"#
                ),
                format!(r#"CREATE INDEX {prefix}tasks_by_kind ON {prefix}tasks (kind, id)"#),
                format!(
                    r#"CREATE INDEX {prefix}tasks_by_abort_requested ON {prefix}tasks (abort_requested, id)"#
                ),
                format!(
                    r#"CREATE INDEX {prefix}tasks_by_background ON {prefix}tasks (background, id)"#
                ),
            ],
        },
    ]
}
