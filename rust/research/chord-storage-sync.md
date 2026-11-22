# Primary-Source Research: `pi-rs-chord` & `pi-rs-durable` SQLite, Links & Sync

Covers Milestone 2.1 (`packages/chord` -> `pi-rs-chord`) and Milestone 2.2 (`packages/durable` -> `pi-rs-durable`). Every claim cites upstream source (`packages/chord/`, `packages/durable/`).

## 1. SQLite Schemas, PRAGMAs & Dialect (`rusqlite` with `bundled` SQLite >= 3.37 + JSON1)

### Connection Setup & Serialization (`packages/chord/src/sqlite/node.ts#L34-L232`)
- **Read-write**: `busy_timeout = 5_000 ms`, `PRAGMA journal_mode = WAL`, `PRAGMA synchronous = NORMAL`, `PRAGMA wal_autocheckpoint = 1_000` (`0` disables). Transactions use `BEGIN IMMEDIATE` / `COMMIT` / `ROLLBACK`; `close()` runs `PRAGMA wal_checkpoint(TRUNCATE)`.
- **Read-only**: No directory creation, no PRAGMAs, no checkpoint on close; deferred `BEGIN`.
- **Prepared statement cache**: LRU capacity `256` (`rusqlite::Connection::set_prepared_statement_cache_capacity(256)`).
- **Tokio boundary**: Serialize all DB access per connection through a dedicated writer/DB actor thread (`spawn_blocking` / single-writer channel) preserving FIFO transaction order (never block Tokio worker threads).
- **No `SAVEPOINT`s**: Explicitly avoided across Durable (`durable/src/storage/sqlite/storage.ts#L155`).

### Migration Metadata Table (`packages/chord/src/sqlite/migrations.ts#L12-L43`, `schema.ts#L2-L9`)
- Identifier validated by `/^[A-Za-z_][A-Za-z0-9_]*$/` (case-insensitive `sqlite_` prefix forbidden). Contiguous integer versions starting at `1`.
- Table DDL: `CREATE TABLE IF NOT EXISTS <table> (singleton INTEGER PRIMARY KEY CHECK (singleton = 1), version INTEGER NOT NULL CHECK (version >= 0)) STRICT` seeded with `(1, 0)`.
- Versions: `chord_store_schema` = **v3**, `telemetry_schema` = **v2**, `${prefix}durable_schema` = **v2**.

### Exact Table Schemas
- **Chord Store (`packages/chord/src/store/schema.ts#L26-L135`, `sql.ts#L23-L103`)**:
  - Verifies `PRAGMA encoding` (`pragma_encoding`) == `"UTF-8"`.
  - System tables (all `STRICT`): `chord_meta` (`(1, 0, 2, 0, 0)`), `chord_tables` (`WITHOUT ROWID`), `chord_documents` (with `host TEXT` added in v3, partial unique live index `WHERE retired_at IS NULL`, `chord_documents_address`, `chord_documents_scope`, `chord_documents_host`), `chord_revisions` (`WITHOUT ROWID`, partial index `WHERE kind = 'base'`), `chord_outbox`.
  - Declared tables: `"<owner>:<table>"` (`STRICT, WITHOUT ROWID` unless single `int` PK), column types `str -> TEXT`, `int -> INTEGER CHECK (col BETWEEN -9007199254740991 AND 9007199254740991)`, `num -> REAL`, `bool -> INTEGER CHECK (col IN (0, 1))`, `json -> "col" TEXT + "col$v" INTEGER`, plus `"$v" INTEGER NOT NULL`.
- **Chord Telemetry (`packages/chord/src/telemetry-sqlite/schema.ts#L14-L41`)**:
  - Does **not** use `STRICT`. v2 stores `started_us INTEGER` / `ended_us INTEGER NOT NULL` (converted from/to `f64` ms). Attribute filtering uses `json_each` distinguishing `entry.type = 'true'/'false'` vs `'integer'/'real'` (`store.ts#L52-L64`).
- **Durable SQLite (`packages/durable/src/storage/sqlite/migrations.ts#L11-L117`)**:
  - All `STRICT` with optional `${prefix}`: `durable_metadata` (**`next_id` stored as `TEXT` `'2'`, NOT `INTEGER`**, because JS `node:sqlite` rejects `INTEGER` > `2^53-1`!), `record_ids`, `conversations`, `entries` (partial index `WHERE head IS NOT NULL`), `tasks` (v2 adds `'sleeping'` to `status` `CHECK`), `submissions`, `documents`, `document_revisions`. All JSON payload columns have `CHECK (json_valid(...))`.
  - **Double-encoded indexed strings (`durable/src/storage/sqlite/rows.ts#L21`)**: `kind` and `request_id` columns store `JSON.stringify(value)` (including surrounding `"..."` quotes, `""` sentinel when absent in `StoreStorage`).

## 2. Chord Link Wire Framing, Binary Codec & Delta Protocol

### Transports (`packages/chord/src/link/`, `node/http/upgrades.ts`)
- **IPC (`ipc.ts`)**: Ordered discrete `Vec<u8>` frames (in Rust workers, length-prefixed Unix socket / stdio pipe replacing Node's V8 structured-clone IPC). Trusted links have unbounded part/decoded limits (`part.ts#L63-L68`).
- **WebSocket (`websocket.ts`, `upgrades.ts#L43,L170-L259`)**:
  - Binary frames only (text frames close the link).
  - **`perMessageDeflate: false`** (explicitly disabled).
  - `max_message_size = 1 MiB` (`DEFAULT_LINK_PAYLOAD`; explicitly override `axum::extract::ws`'s 64 MiB default).
  - Untrusted part limits: `{ part: 16 MiB, decoded: 64 MiB }`.
  - Outbound backpressure cap: terminate connection if unsent buffer exceeds `4 MiB` (`MAX_UNSENT_BYTES`).
  - Heartbeat `Channel.tick` (`ping` / `pong` silence timeout, `15_000 ms` for plugin sockets).

### Binary Codec & Part Compression (`link/binary.ts`, `part.ts`, `values.ts`, `node/compression.ts`)
- Byte 0: `kind_index` (`0..=18`) OR'd with `0x80` (`ORIGIN`), `0x40` (`TRACE`, only valid on `hold`/`call`), `0x20` (`OLD` on `call` / event-ack on `ack`).
- Unsigned LEB128 varints: max **8 bytes**, value `<= 2^53 - 1` (`MAX_SAFE_INTEGER`). **Trap**: upstream `Writer` encodes long strings (`>= 4096` UTF-16 units) with a **non-minimal 5-byte padded LEB128 varint** (`LENGTH_SLOT = 5`, `binary.ts#L431-L435`); Rust decoder must accept non-minimal LEB128 up to 8 bytes and reject trailing bytes at `Reader::end()`.
- UUIDs (`incarnation`, `chain`): 16 raw bytes; `fingerprint`: 8 raw bytes.
- `Part`: prefix varint `(byte_len << 1) | deflated_bit`. Compression uses **raw deflate (RFC 1951)** (`flate2::DeflateEncoder` at level `1`, NOT zlib/gzip) ONLY when UTF-8 length > `16 KiB` AND `deflated.len() < raw.len()`. Relays forward parts opaque/uninflated.
- Binary payload segments: JSON replaces `Uint8Array` leaves with `{"$bytes": i}` (own key `$bytes` forbidden in user objects), followed by `flags` (`0`/`1`/`3`), part, segment count, segment lengths, and concatenated raw bytes.

### Delta Encoding (`packages/chord/src/delta/`)
- Decoded `Op` tuples: `["r", val]`, `["s", path, val]`, `["d", path]`, `["a", path, text]`, `["t", path, count]`, `["p", path, idx, rem, items]`, `["m", path, perm]`.
- Wire stream state: `["r", ...]` resets path dictionary; paths interned on **2nd** non-consecutive use via `["#", id, path]`; consecutive ops on same path omit path ref.
- **Traps**:
  - `["t", path, count]` truncates **`count` UTF-16 code units** (non-BMP chars count as 2!), NOT UTF-8 bytes or scalar chars (`delta/index.ts#L414`).
  - Must still reject `"__proto__"`, `"constructor"`, `"prototype"` path segments (`UnsafePathError`, `index.ts#L145`) for TS wire compatibility, and enforce dense arrays (`index <= len`), finite numbers, and `-0.0` -> `0`.
  - Slow-consumer `PendingOps` folds leaf ops and collapses past `MAX_PENDING_OPS = 10_000`; `diff` caps at `MAX_DELTA_OPERATIONS = 4_096` and falls back to `["r", after]`.

## 3. Sync Outbox, Slots & Archive Snapshots (`packages/chord/src/store/`, `src/sync/`)

- **Outbox (`chord_outbox`)**: `OUTBOX_FORMAT = 1`, JSON array of `StoreWrite` operations (table mutations with recorded `count` for divergence checking + document `create`/`change`/`retire`).
- **Sync batches (`sync/batch.ts`, `lines.ts`, `remote.ts`, `filesystem.ts`)**:
  - **gzip-compressed** (`flate2::GzEncoder`, NOT raw deflate!) UTF-8 JSONL (`LOG_VERSION = 2`, `SYNC_PROTOCOL = 1`): `{"v":2,"seq":1,"nextId":2,"writes":[...]}`.
  - Strictly increasing `seq >= 1`, trailing `\n` required; upload chunks at `8 MiB` (`OUTBOX_PAGE = 100`, `PAGE_BYTES = 1 MiB`), pull applies in `4 MiB` chunks.
  - `FileSystemSyncRemote` paths: `{root}/{session}/slots/{seq:016}.json`, `batches/{seq:016}.batch`, `snapshots/{seq:016}.batch`, `meta.json`; local state atomic `.json.tmp` rename.
- **Archive / Snapshot (`store/archive.ts`, `ARCHIVE_FORMAT = 1`)**: gzip-compressed JSONL in canonical order: `manifest` (`schema: 3`) -> sorted tables (`table`, key-ordered `put` rows, `index`) -> `id`-ordered documents (`document.create` without `base`, then `seq`-ordered `document.revision`) -> `end` with exact intermediate part count.
