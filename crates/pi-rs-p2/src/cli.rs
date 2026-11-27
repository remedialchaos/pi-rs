//! Operational CLI subcommands for `p2` matching upstream `apps/p2/scripts/*`.

use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    error::Error as StdError,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    process::ExitCode,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use indexmap::IndexMap;
use pi_rs_ai::auth::LoginOptions;
use pi_rs_chord::{
    context::Context,
    node::{
        app::{FileWrite, Files, FilesCaps, private_directory},
        http::connect_websocket,
        workers::ProcessLock,
    },
    remote::{RemoteOptions, RemotePeer},
    sqlite::{Mode, Options as SqliteOptions, SqliteDatabase, SqliteValue},
    state::Batch,
    store::{Query, StoreEngine, TableDefinition, TableHistory},
    telemetry_sqlite::{assert_store, find_traces},
};
use pi_rs_durable::{
    DocumentAddress, DocumentContent, DocumentCreate, DocumentHistory, DocumentPoint,
    DocumentQuery, DocumentRecord, DocumentScope, EntryQuery, EntryRecord, ScanOrder,
    SubmissionQuery, SubmissionRecord, TaskQuery, TaskRecord,
    storage::{CommitResult, Storage, StorageWrite, sqlite::SqliteStorage},
};
use pi_rs_telemetry::TraceQuery;
use regex::Regex;
use reqwest::{Client, Url, header};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{
    api::v1::p2_contracts::lookup,
    plugins::{
        radius::login::{gateway_from_env, login},
        shard_a::{sessions::catalog::CatalogTable, sharing::store::Tables as ShareTablesDef},
        subagents::children::child_rows_table,
    },
    runtime::{
        address::session_address,
        host_dirs,
        local_owner::{LOCAL_CHALLENGE_PATH, LOCAL_CLAIM_PATH, local_owner_proof},
        session_store::durable_storage,
    },
};

pub type CliError = Box<dyn StdError + Send + Sync>;

const JOURNAL_SUFFIXES: [&str; 3] = ["", "-wal", "-shm"];
const MIGRATION_COMMITS_PER_CALL: usize = 200;
const FILES_CHUNK_BYTES: usize = 4 * 1024 * 1024;
const VERIFY_NEWEST_LIMIT: usize = 10;

pub fn maybe_run() -> Option<ExitCode> {
    let mut args = env::args();
    let _program = args.next()?;
    let command = args.next()?;
    match command.as_str() {
        "owner-link" | "reload" | "migrate-data" | "radius:login" | "data-report"
        | "verify-sessions" | "traces" => {}
        _ => return None,
    }
    let rest: Vec<String> = args.collect();
    let code = run_subcommand(&command, &rest);
    Some(code)
}

fn run_subcommand(command: &str, args: &[String]) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(error) => {
            eprintln!("Failed to initialize runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(async move {
        match command {
            "owner-link" => match owner_link_command(args).await {
                Ok(link) => {
                    println!("{link}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("{error}");
                    ExitCode::FAILURE
                }
            },
            "reload" => match reload_command(args).await {
                Ok((ok, message)) => {
                    if ok {
                        println!("{message}");
                        ExitCode::SUCCESS
                    } else {
                        eprintln!("{message}");
                        ExitCode::FAILURE
                    }
                }
                Err(error) => {
                    eprintln!("Reload refused: {error}");
                    ExitCode::FAILURE
                }
            },
            "radius:login" => match radius_login_command(args).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("{error}");
                    ExitCode::FAILURE
                }
            },
            "traces" => match traces_command(args).await {
                Ok(output) => {
                    print!("{output}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("{error}");
                    ExitCode::FAILURE
                }
            },
            "data-report" => match data_report_command(args).await {
                Ok((ok, output)) => {
                    println!("{output}");
                    if ok {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::FAILURE
                    }
                }
                Err(error) => {
                    eprintln!("{error}");
                    ExitCode::FAILURE
                }
            },
            "migrate-data" => match migrate_data_command(args).await {
                Ok((ok, output)) => {
                    println!("{output}");
                    if ok {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::FAILURE
                    }
                }
                Err(error) => {
                    eprintln!("{error}");
                    ExitCode::FAILURE
                }
            },
            "verify-sessions" => match verify_sessions_command(args).await {
                Ok((ok, output)) => {
                    println!("{output}");
                    if ok {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::FAILURE
                    }
                }
                Err(error) => {
                    eprintln!("{error}");
                    ExitCode::FAILURE
                }
            },
            _ => ExitCode::FAILURE,
        }
    })
}

pub fn data_directory() -> Result<PathBuf, CliError> {
    if let Some(dir) = env::var_os("P2_DATA_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let home = env::home_dir().ok_or("Cannot determine home directory")?;
    Ok(home.join(".pi").join("p2"))
}

fn option_value(args: &[String], name: &str) -> Result<Option<String>, CliError> {
    let Some(pos) = args.iter().position(|arg| arg == name) else {
        return Ok(None);
    };
    let val = args
        .get(pos + 1)
        .ok_or_else(|| format!("{name} requires a value"))?;
    Ok(Some(val.clone()))
}

fn option_values(args: &[String], name: &str) -> Result<Vec<String>, CliError> {
    let mut values = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == name {
            let val = args
                .get(i + 1)
                .ok_or_else(|| format!("{name} requires a value"))?;
            values.push(val.clone());
            i += 2;
        } else {
            i += 1;
        }
    }
    Ok(values)
}

fn positional_arg(args: &[String]) -> Option<&str> {
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--dry-run" || arg == "--errors" || arg == "--restart" {
            i += 1;
        } else if arg.starts_with("--") {
            i += 2;
        } else {
            return Some(arg.as_str());
        }
    }
    None
}

fn port_from_env() -> Result<u16, CliError> {
    match env::var("PORT") {
        Ok(raw) => {
            let port: u16 = raw.parse().map_err(|_| "Invalid PORT")?;
            if port == 0 {
                return Err("Invalid PORT".into());
            }
            Ok(port)
        }
        Err(_) => Ok(8788),
    }
}

pub fn parse_owner_origin(raw: &str) -> Result<Url, CliError> {
    let url = Url::parse(raw).map_err(|_| "Invalid base URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
    {
        return Err("Invalid base URL".into());
    }
    let origin = url.origin().ascii_serialization();
    Ok(Url::parse(&origin)?)
}

pub async fn mint_owner_link(
    loopback: &Url,
    data_dir: &Path,
    public_origin: &Url,
) -> Result<Url, CliError> {
    let port = loopback
        .port_or_known_default()
        .ok_or("Missing server port")?;
    let owner_key = fs::read_to_string(data_dir.join("owner-key"))
        .map_err(|_| "Missing owner key; start p2 first")?;
    let origin = loopback.origin().ascii_serialization();
    let client = Client::builder().timeout(Duration::from_secs(10)).build()?;

    let challenge_res = client
        .post(loopback.join(LOCAL_CHALLENGE_PATH)?)
        .header(header::ORIGIN, &origin)
        .send()
        .await?;
    if !challenge_res.status().is_success() {
        return Err(format!("Local sign-in refused ({})", challenge_res.status()).into());
    }
    let challenge_json: Value = challenge_res.json().await?;
    let challenge = challenge_json["challenge"]
        .as_str()
        .ok_or("Missing local sign-in challenge")?;
    let proof = local_owner_proof(owner_key.trim(), port, challenge);

    let claim_res = client
        .post(loopback.join(LOCAL_CLAIM_PATH)?)
        .header(header::ORIGIN, &origin)
        .json(&json!({ "challenge": challenge, "proof": proof }))
        .send()
        .await?;
    if !claim_res.status().is_success() {
        return Err(format!("Local claim failed ({})", claim_res.status()).into());
    }
    let cookie =
        extract_cookie(&claim_res, &format!("p2_owner_{port}=")).ok_or("Missing owner cookie")?;

    let token_res = client
        .get(loopback.join("/owner-token")?)
        .header(header::ORIGIN, &origin)
        .header(header::COOKIE, &cookie)
        .header("Sec-Fetch-Site", "same-origin")
        .send()
        .await?;
    if !token_res.status().is_success() {
        return Err(format!("Owner token request failed ({})", token_res.status()).into());
    }
    let token_json: Value = token_res.json().await?;
    let token = token_json["token"].as_str().ok_or("Missing owner token")?;
    let mut link = public_origin.join("/owner")?;
    link.set_fragment(Some(token));
    Ok(link)
}

fn extract_cookie(response: &reqwest::Response, prefix: &str) -> Option<String> {
    let mut fallback = None;
    for value in response.headers().get_all(header::SET_COOKIE) {
        let Ok(raw) = value.to_str() else {
            continue;
        };
        let Some(pair) = raw.split(';').next() else {
            continue;
        };
        if pair.starts_with(prefix) {
            return Some(pair.to_owned());
        }
        if fallback.is_none() && pair.contains('=') {
            fallback = Some(pair.to_owned());
        }
    }
    fallback
}

pub async fn owner_link_command(args: &[String]) -> Result<String, CliError> {
    if args.len() > 1 {
        return Err("Usage: p2 owner-link [base-url]".into());
    }
    let port = port_from_env()?;
    let default_base = format!("http://127.0.0.1:{port}");
    let raw_base = args
        .first()
        .cloned()
        .or_else(|| env::var("P2_PUBLIC_URL").ok())
        .unwrap_or(default_base);
    let public = parse_owner_origin(&raw_base)?;
    let loopback = Url::parse(&format!("http://127.0.0.1:{port}"))?;
    let link = mint_owner_link(&loopback, &data_directory()?, &public).await?;
    Ok(link.into())
}

pub async fn reload_command(args: &[String]) -> Result<(bool, String), CliError> {
    if args.iter().any(|arg| arg == "--restart") && args.len() != 1 {
        return Err("--restart cannot be combined with plugin names".into());
    }
    #[cfg(unix)]
    {
        use crate::hosts::dev::{DevCommand, channel};
        let command = if args.first().map(String::as_str) == Some("--restart") {
            DevCommand::Restart
        } else {
            DevCommand::Reload {
                plugins: (!args.is_empty()).then(|| args.to_vec()),
            }
        };
        let result = channel::send(&data_directory()?, &command).await?;
        Ok((result.ok, result.message))
    }
    #[cfg(not(unix))]
    {
        let _ = args;
        Err("reload is only supported on Unix hosts".into())
    }
}

pub async fn radius_login_command(args: &[String]) -> Result<(), CliError> {
    let data_dir = match option_value(args, "--data")? {
        Some(dir) => PathBuf::from(dir),
        None => data_directory()?,
    };
    private_directory(&data_dir)?;
    let gateway = gateway_from_env()?;
    let gateway_display = gateway.as_str().to_owned();
    login(
        &data_dir,
        gateway,
        |uri, code| {
            println!("\nOpen {uri} and enter {code}\n");
        },
        &LoginOptions::default(),
    )
    .await?;
    println!("Signed in to Radius at {gateway_display}.");
    Ok(())
}

pub async fn traces_command(args: &[String]) -> Result<String, CliError> {
    let data_dir = match option_value(args, "--data")? {
        Some(dir) => PathBuf::from(dir),
        None => data_directory()?,
    };
    let name = option_value(args, "--name")?;
    let attr_pairs = option_values(args, "--attr")?;
    let errors = args.iter().any(|arg| arg == "--errors");
    let since = option_value(args, "--since")?;
    let limit = option_value(args, "--limit")?;

    let mut attributes = IndexMap::new();
    for pair in attr_pairs {
        let Some((k, v)) = pair.split_once('=') else {
            return Err(format!("--attr {pair}: expected key=value").into());
        };
        if k.is_empty() {
            return Err(format!("--attr {pair}: expected key=value").into());
        }
        attributes.insert(
            k.to_owned(),
            pi_rs_telemetry::AttributeValue::String(v.to_owned()),
        );
    }
    let start_min = match since {
        Some(raw) => {
            let minutes: f64 = raw.parse().map_err(|_| format!("Invalid --since {raw}"))?;
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs_f64() * 1000.0)
                .unwrap_or(0.0);
            Some(now_ms - minutes * 60_000.0)
        }
        None => None,
    };
    let limit = match limit {
        Some(raw) => raw
            .parse::<usize>()
            .map_err(|_| format!("Invalid --limit {raw}"))?,
        None => 10,
    };

    let store_path = host_dirs::telemetry_store(&data_dir);
    let db = SqliteDatabase::open(
        &store_path,
        SqliteOptions {
            mode: Mode::ReadOnly,
            ..Default::default()
        },
    )
    .await?;
    let result = async {
        assert_store(&db).await?;
        let page = find_traces(
            &db,
            TraceQuery {
                name,
                attributes,
                errors,
                start_min,
                start_max: None,
                limit: Some(limit),
                cursor: None,
            },
        )
        .await?;
        let mut out = String::new();
        for trace in page.traces {
            let rendered = pi_rs_telemetry::render_trace(&trace.spans).map_err(|err| {
                format!("Cannot render trace {}: {}", trace.trace_id, err.message)
            })?;
            out.push_str(&format!("trace {}\n{rendered}\n\n", trace.trace_id));
        }
        Ok::<_, CliError>(out)
    }
    .await;
    let _ = db.close().await;
    result
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationReport {
    pub entries: usize,
    pub newest: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentReport {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    pub archived: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionShareReport {
    pub links: Vec<String>,
    pub guests: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionReport {
    pub title: String,
    pub location: String,
    pub archived: bool,
    pub updated_at: i64,
    pub conversations: BTreeMap<String, ConversationReport>,
    pub subagents: Vec<SubagentReport>,
    pub shares: SessionShareReport,
    pub artifacts: BTreeMap<String, usize>,
    pub uploads: usize,
    pub downloads: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardReport {
    pub port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub public: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataReport {
    pub sessions: BTreeMap<String, SessionReport>,
    pub envs: Vec<String>,
    pub forwards: Vec<ForwardReport>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredChildReport {
    name: String,
    kind: Option<String>,
    archived: Option<bool>,
    cwd: Option<String>,
    location: Option<StoredLocationReport>,
}

#[derive(Deserialize)]
struct StoredLocationReport {
    env: Option<String>,
    cwd: String,
}

#[derive(Deserialize)]
struct StoredForwardReport {
    port: u16,
    name: Option<String>,
    public: Option<bool>,
}

fn located(env: Option<&str>, cwd: &str) -> String {
    match env {
        Some(env) if !env.is_empty() => format!("{env}:{cwd}"),
        _ => cwd.to_owned(),
    }
}

fn session_id_from_address_key(key: &str) -> String {
    key.strip_prefix("session|").unwrap_or(key).to_owned()
}

fn val_text(val: &SqliteValue) -> Option<&str> {
    match val {
        SqliteValue::Text(s) => Some(s.as_str()),
        _ => None,
    }
}

fn val_int(val: &SqliteValue) -> Option<i64> {
    match val {
        SqliteValue::Integer(n) => Some(*n),
        _ => None,
    }
}

async fn read_rows_if_table(
    path: &Path,
    table: &str,
    sql: &str,
) -> Result<Vec<Vec<SqliteValue>>, CliError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let db = SqliteDatabase::open(
        path,
        SqliteOptions {
            mode: Mode::ReadOnly,
            ..Default::default()
        },
    )
    .await?;
    let table = table.to_owned();
    let sql = sql.to_owned();
    let result = db
        .call(move |conn| {
            let exists: bool = conn
                .prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?")?
                .query([table.as_str()])?
                .next()?
                .is_some();
            if !exists {
                return Ok(Vec::new());
            }
            let mut stmt = conn.prepare(&sql)?;
            let col_count = stmt.column_count();
            let mut rows = stmt.query([])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                let mut cells = Vec::with_capacity(col_count);
                for idx in 0..col_count {
                    cells.push(row.get::<_, SqliteValue>(idx)?);
                }
                out.push(cells);
            }
            Ok(out)
        })
        .await;
    let _ = db.close().await;
    Ok(result?)
}

async fn table_exists(path: &Path, table: &str) -> Result<bool, CliError> {
    if !path.exists() {
        return Ok(false);
    }
    let db = SqliteDatabase::open(
        path,
        SqliteOptions {
            mode: Mode::ReadOnly,
            ..Default::default()
        },
    )
    .await?;
    let table = table.to_owned();
    let result = db
        .call(move |conn| {
            Ok(conn
                .prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?")?
                .query([table.as_str()])?
                .next()?
                .is_some())
        })
        .await;
    let _ = db.close().await;
    Ok(result?)
}

async fn catalog_rows(
    data_dir: &Path,
) -> Result<Vec<(String, String, String, Option<String>, i64, bool)>, CliError> {
    let server_store = host_dirs::host_store(&host_dirs::server_directory(data_dir));
    let stored = read_rows_if_table(
        &server_store,
        "p2.catalog:sessions",
        r#"SELECT session, title, cwd, env, updatedAt, archivedAt FROM "p2.catalog:sessions" ORDER BY session"#,
    )
    .await?;
    let rows = if !stored.is_empty() {
        stored
            .into_iter()
            .filter_map(|row| {
                let id = session_id_from_address_key(val_text(row.first()?)?);
                let title = val_text(row.get(1)?)?.to_owned();
                let cwd = val_text(row.get(2)?)?.to_owned();
                let env = val_text(row.get(3)?).map(str::to_owned);
                let updated_at = val_int(row.get(4)?)?;
                let archived = !matches!(row.get(5)?, SqliteValue::Null);
                Some((id, title, cwd, env, updated_at, archived))
            })
            .collect()
    } else {
        let legacy = read_rows_if_table(
            &data_dir.join("catalog.sqlite"),
            "sessions",
            "SELECT id, title, cwd, env, updatedAt, archivedAt FROM sessions ORDER BY id",
        )
        .await?;
        legacy
            .into_iter()
            .filter_map(|row| {
                let id = val_text(row.first()?)?.to_owned();
                let title = val_text(row.get(1)?)?.to_owned();
                let cwd = val_text(row.get(2)?)?.to_owned();
                let env = val_text(row.get(3)?).map(str::to_owned);
                let updated_at = val_int(row.get(4)?)?;
                let archived = !matches!(row.get(5)?, SqliteValue::Null);
                Some((id, title, cwd, env, updated_at, archived))
            })
            .collect()
    };
    Ok(rows)
}

async fn child_rows_map(
    data_dir: &Path,
) -> Result<BTreeMap<String, Vec<SubagentReport>>, CliError> {
    let server_store = host_dirs::host_store(&host_dirs::server_directory(data_dir));
    let stored = read_rows_if_table(
        &server_store,
        "p2.child-store:children",
        r#"SELECT session, children FROM "p2.child-store:children""#,
    )
    .await?;
    let raw_rows = if !stored.is_empty() {
        stored
            .into_iter()
            .filter_map(|row| {
                let session = session_id_from_address_key(val_text(row.first()?)?);
                let json_str = val_text(row.get(1)?)?.to_owned();
                Some((session, json_str))
            })
            .collect::<Vec<_>>()
    } else {
        read_rows_if_table(
            &data_dir.join("subagents.sqlite"),
            "children",
            "SELECT session, value FROM children",
        )
        .await?
        .into_iter()
        .filter_map(|row| {
            let session = val_text(row.first()?)?.to_owned();
            let json_str = val_text(row.get(1)?)?.to_owned();
            Some((session, json_str))
        })
        .collect::<Vec<_>>()
    };

    let mut map = BTreeMap::new();
    for (session, json_str) in raw_rows {
        let items: Vec<StoredChildReport> = serde_json::from_str(&json_str)?;
        let mut children: Vec<SubagentReport> = items
            .into_iter()
            .filter(|child| child.kind.as_deref() != Some("agent"))
            .map(|child| SubagentReport {
                name: child.name,
                location: child
                    .location
                    .map(|loc| located(loc.env.as_deref(), &loc.cwd))
                    .or(child.cwd),
                archived: child.archived == Some(true),
            })
            .collect();
        children.sort_by(|a, b| a.name.cmp(&b.name));
        map.insert(session, children);
    }
    Ok(map)
}

async fn shares_by_session(
    data_dir: &Path,
) -> Result<BTreeMap<String, SessionShareReport>, CliError> {
    let mut shares: BTreeMap<String, SessionShareReport> = BTreeMap::new();
    let server_store = host_dirs::host_store(&host_dirs::server_directory(data_dir));
    let new_links = read_rows_if_table(
        &server_store,
        "p2.shares:links",
        r#"SELECT session, mode FROM "p2.shares:links" ORDER BY session, mode"#,
    )
    .await?;
    if !new_links.is_empty() {
        for row in new_links {
            if let (Some(session), Some(mode)) = (
                row.first().and_then(val_text),
                row.get(1).and_then(val_text),
            ) {
                shares
                    .entry(session_id_from_address_key(session))
                    .or_default()
                    .links
                    .push(mode.to_owned());
            }
        }
        let new_grants = read_rows_if_table(
            &server_store,
            "p2.shares:grants",
            r#"SELECT session, mode FROM "p2.shares:grants" WHERE removed=0 ORDER BY session, mode"#,
        )
        .await?;
        for row in new_grants {
            if let (Some(session), Some(mode)) = (
                row.first().and_then(val_text),
                row.get(1).and_then(val_text),
            ) {
                shares
                    .entry(session_id_from_address_key(session))
                    .or_default()
                    .guests
                    .push(mode.to_owned());
            }
        }
        return Ok(shares);
    }

    let legacy = data_dir.join("shares.sqlite");
    for row in read_rows_if_table(
        &legacy,
        "links",
        "SELECT session, mode FROM links ORDER BY session, mode",
    )
    .await?
    {
        if let (Some(session), Some(mode)) = (
            row.first().and_then(val_text),
            row.get(1).and_then(val_text),
        ) {
            shares
                .entry(session.to_owned())
                .or_default()
                .links
                .push(mode.to_owned());
        }
    }
    let has_grants = table_exists(&legacy, "grants").await?;
    let grants_rows = if has_grants {
        read_rows_if_table(
            &legacy,
            "grants",
            "SELECT session, mode FROM grants WHERE removed=0 ORDER BY session, mode",
        )
        .await?
    } else {
        read_rows_if_table(
            &legacy,
            "guests",
            "SELECT session, mode FROM guests JOIN links ON links.id=guests.linkId WHERE removed=0 ORDER BY session, mode",
        )
        .await?
    };
    for row in grants_rows {
        if let (Some(session), Some(mode)) = (
            row.first().and_then(val_text),
            row.get(1).and_then(val_text),
        ) {
            shares
                .entry(session.to_owned())
                .or_default()
                .guests
                .push(mode.to_owned());
        }
    }
    Ok(shares)
}

async fn conversations_of(
    data_dir: &Path,
    id: &str,
) -> Result<BTreeMap<String, ConversationReport>, CliError> {
    let old = data_dir.join("sessions").join(format!("{id}.sqlite"));
    let (path, table) = if old.exists() {
        (old, "entries")
    } else {
        (
            host_dirs::host_store(&host_dirs::session_directory(data_dir, id)?),
            "p2.durable:entries",
        )
    };
    let sql = format!(
        r#"SELECT conversation_id, count(*), max(id) FROM "{table}" GROUP BY conversation_id ORDER BY conversation_id"#
    );
    let mut conversations = BTreeMap::new();
    for row in read_rows_if_table(&path, table, &sql).await? {
        let Some(conv_id) = row.first().and_then(val_int) else {
            continue;
        };
        let Some(entries) = row.get(1).and_then(val_int) else {
            continue;
        };
        let Some(newest) = row.get(2).and_then(val_int) else {
            continue;
        };
        conversations.insert(
            conv_id.to_string(),
            ConversationReport {
                entries: usize::try_from(entries).unwrap_or_default(),
                newest,
            },
        );
    }
    Ok(conversations)
}

fn dir_entries(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| !name.starts_with('.'))
        .collect();
    names.sort();
    names
}

async fn files_of(
    data_dir: &Path,
    id: &str,
) -> Result<(BTreeMap<String, usize>, usize, usize), CliError> {
    let old = data_dir.join("sessions").join(format!("{id}.sqlite"));
    if old.exists() {
        let mut artifacts = BTreeMap::new();
        let artifacts_dir = data_dir.join("artifacts").join(id);
        for name in dir_entries(&artifacts_dir) {
            let count = dir_entries(&artifacts_dir.join(&name))
                .into_iter()
                .filter(|v| {
                    artifacts_dir
                        .join(&name)
                        .join(v)
                        .join("version.json")
                        .exists()
                })
                .count();
            artifacts.insert(name, count);
        }
        let uploads = dir_entries(&data_dir.join("uploads").join(id)).len();
        let downloads_dir = data_dir.join("downloads").join(id);
        let downloads = dir_entries(&downloads_dir)
            .into_iter()
            .filter(|d| downloads_dir.join(d).join("metadata.json").exists())
            .count();
        return Ok((artifacts, uploads, downloads));
    }

    let store = host_dirs::host_store(&host_dirs::session_directory(data_dir, id)?);
    let rows = read_rows_if_table(
        &store,
        "chord.app.files:files",
        r#"SELECT space, path FROM "chord.app.files:files" ORDER BY path"#,
    )
    .await?;
    let mut artifacts = BTreeMap::new();
    let mut uploads = 0usize;
    let mut downloads = 0usize;
    for row in rows {
        let Some(space) = row.first().and_then(val_text) else {
            continue;
        };
        let path = row.get(1).and_then(val_text).unwrap_or_default();
        match space {
            "artifacts" => {
                *artifacts.entry(path.to_owned()).or_insert(0) += 1;
            }
            "uploads" => uploads += 1,
            "downloads" => downloads += 1,
            _ => {}
        }
    }
    Ok((artifacts, uploads, downloads))
}

fn read_json_map(path: &Path) -> Result<Option<Map<String, Value>>, CliError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

pub async fn data_report(data_dir: &Path) -> Result<DataReport, CliError> {
    let catalog = catalog_rows(data_dir).await?;
    let children = child_rows_map(data_dir).await?;
    let shares = shares_by_session(data_dir).await?;

    let mut sessions = BTreeMap::new();
    for (id, title, cwd, env, updated_at, archived) in catalog {
        let conversations = conversations_of(data_dir, &id).await?;
        let (artifacts, uploads, downloads) = files_of(data_dir, &id).await?;
        sessions.insert(
            id.clone(),
            SessionReport {
                title,
                location: located(env.as_deref(), &cwd),
                archived,
                updated_at,
                conversations,
                subagents: children.get(&id).cloned().unwrap_or_default(),
                shares: shares.get(&id).cloned().unwrap_or_default(),
                artifacts,
                uploads,
                downloads,
            },
        );
    }

    let mut envs: Vec<String> = read_json_map(&data_dir.join("envs.json"))?
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    envs.sort();

    let mut forwards: Vec<ForwardReport> = read_json_map(&data_dir.join("forwards.json"))?
        .map(|m| {
            m.into_values()
                .filter_map(|v| serde_json::from_value::<StoredForwardReport>(v).ok())
                .map(|f| ForwardReport {
                    port: f.port,
                    name: f.name,
                    public: f.public == Some(true),
                })
                .collect()
        })
        .unwrap_or_default();
    forwards.sort_by_key(|f| f.port);

    Ok(DataReport {
        sessions,
        envs,
        forwards,
    })
}

pub fn compare_reports(before: &Value, after: &Value, path: &str) -> Vec<String> {
    if before == after {
        return Vec::new();
    }
    if let (Some(a), Some(b)) = (before.as_object(), after.as_object()) {
        let mut keys: BTreeSet<&str> = a.keys().map(String::as_str).collect();
        keys.extend(b.keys().map(String::as_str));
        let mut diffs = Vec::new();
        for key in keys {
            let av = a.get(key).unwrap_or(&Value::Null);
            let bv = b.get(key).unwrap_or(&Value::Null);
            diffs.extend(compare_reports(av, bv, &format!("{path}/{key}")));
        }
        return diffs;
    }
    let label = if path.is_empty() { "/" } else { path };
    vec![format!(
        "{label}: {} -> {}",
        serde_json::to_string(before).unwrap_or_default(),
        serde_json::to_string(after).unwrap_or_default()
    )]
}

pub async fn data_report_command(args: &[String]) -> Result<(bool, String), CliError> {
    let data_dir = match option_value(args, "--data")? {
        Some(dir) => PathBuf::from(dir),
        None => data_directory()?,
    };
    let report = data_report(&data_dir).await?;
    let Some(compare_path) = option_value(args, "--compare")? else {
        return Ok((true, serde_json::to_string_pretty(&report)?));
    };
    let before: Value = serde_json::from_slice(&fs::read(compare_path)?)?;
    let after = serde_json::to_value(&report)?;
    let diffs = compare_reports(&before, &after, "");
    let ok = diffs.is_empty();
    let mut lines = diffs;
    lines.push(if ok {
        "no differences".into()
    } else {
        format!("{} differences", lines.len())
    });
    Ok((ok, lines.join("\n")))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum CatalogMigration {
    Migrated { sessions: usize },
    Done,
    Absent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum FileMigration {
    Migrated { rows: usize },
    Done,
    Absent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ServerDataMigration {
    pub shares: FileMigration,
    pub subagents: FileMigration,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SessionMigrationError {
    pub id: String,
    pub error: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SessionsMigrationSummary {
    pub migrated: Vec<String>,
    pub skipped: Vec<String>,
    pub failed: Vec<SessionMigrationError>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationSummary {
    pub data_dir: String,
    pub dry_run: bool,
    pub server: &'static str,
    pub catalog: CatalogMigration,
    pub server_data: ServerDataMigration,
    pub sessions: SessionsMigrationSummary,
    pub entries: usize,
    pub documents: usize,
    pub files: usize,
    pub bytes: u64,
}

fn copy_sqlite_with_journal(from: &Path, to: &Path) -> Result<(), CliError> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    for suffix in JOURNAL_SUFFIXES {
        let src = PathBuf::from(format!("{}{}", from.display(), suffix));
        if src.exists() {
            let dst = PathBuf::from(format!("{}{}", to.display(), suffix));
            fs::copy(src, dst)?;
        }
    }
    Ok(())
}

fn move_backup_files(
    data_dir: &Path,
    backup_dir: &str,
    relative_paths: &[PathBuf],
) -> Result<(), CliError> {
    for rel in relative_paths {
        let is_sqlite = rel.extension().and_then(|e| e.to_str()) == Some("sqlite");
        let suffixes: &[&str] = if is_sqlite { &JOURNAL_SUFFIXES } else { &[""] };
        for suffix in suffixes {
            let from = PathBuf::from(format!("{}{}", data_dir.join(rel).display(), suffix));
            if !from.exists() {
                continue;
            }
            let to = PathBuf::from(format!(
                "{}{}",
                data_dir.join(backup_dir).join(rel).display(),
                suffix
            ));
            if let Some(parent) = to.parent() {
                private_directory(parent)?;
            }
            fs::rename(from, to)?;
        }
    }
    Ok(())
}

async fn store_export_bytes(path: &Path) -> Result<Vec<u8>, CliError> {
    let db = SqliteDatabase::open(path, SqliteOptions::default()).await?;
    let store = StoreEngine::open(db.clone()).await?;
    let bytes = store.export_gzip().await?;
    let _ = db.close().await;
    Ok(bytes)
}

async fn migrate_server_store(
    data_dir: &Path,
    work: &Path,
    dry_run: bool,
) -> Result<&'static str, CliError> {
    let old = data_dir.join("server.sqlite");
    let server_dir = host_dirs::server_directory(work);
    let target = host_dirs::host_store(&server_dir);
    let real_target = host_dirs::host_store(&host_dirs::server_directory(data_dir));
    if !old.exists() {
        return Ok(if real_target.exists() {
            "done"
        } else {
            "absent"
        });
    }
    if !dry_run && target.exists() {
        move_backup_files(
            data_dir,
            "pre-files-backup",
            &[PathBuf::from("server.sqlite")],
        )?;
        return Ok("done");
    }

    let scratch = tempfile::tempdir()?;
    let old_copy = scratch.path().join("old.sqlite");
    copy_sqlite_with_journal(&old, &old_copy)?;
    let expected = store_export_bytes(&old_copy).await?;

    let staging = server_dir.join(".store-migrating");
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    private_directory(&staging)?;
    let staged_store = host_dirs::host_store(&staging);
    copy_sqlite_with_journal(&old, &staged_store)?;
    let db = SqliteDatabase::open(&staged_store, SqliteOptions::default()).await?;
    let _store = StoreEngine::open(db.clone()).await?;
    db.close().await?;
    if store_export_bytes(&staged_store).await? != expected {
        return Err("server.sqlite differs after the copy".into());
    }
    private_directory(&server_dir)?;
    for suffix in JOURNAL_SUFFIXES {
        let src = PathBuf::from(format!("{}{}", staged_store.display(), suffix));
        if src.exists() {
            let dst = PathBuf::from(format!("{}{}", target.display(), suffix));
            fs::rename(src, dst)?;
        }
    }
    fs::remove_dir_all(&staging)?;
    if !dry_run {
        move_backup_files(
            data_dir,
            "pre-files-backup",
            &[PathBuf::from("server.sqlite")],
        )?;
    }
    Ok("migrated")
}

async fn table_put_all(
    server_dir: &Path,
    owner: &str,
    histories: &[TableHistory],
    check_table: &TableDefinition,
    rows_to_put: Vec<(&TableDefinition, Vec<Value>)>,
) -> Result<(bool, BTreeMap<String, Vec<Value>>), CliError> {
    let store = host_dirs::open_host_store(server_dir).await?;
    let result = async {
        store
            .prepare_tables(owner, histories, &Context::background())
            .await?;
        let existing_count = store.count(Query::new(owner, check_table.clone())?).await?;
        let write_now = !rows_to_put.iter().all(|(_, rows)| rows.is_empty());
        let mut kept = false;
        if write_now {
            if existing_count > 0 {
                kept = true;
            } else {
                let mut writes = Vec::new();
                for (table, rows) in &rows_to_put {
                    for row in rows {
                        writes.push(table.put(owner, row.clone())?);
                    }
                }
                if !writes.is_empty() {
                    store.commit(writes).await?;
                }
            }
        }
        let mut read_back = BTreeMap::new();
        for (table, _) in &rows_to_put {
            let mut query = Query::new(owner, (*table).clone())?.with_limit(1000)?;
            let mut all = Vec::new();
            loop {
                let page = store.read(query.clone()).await?;
                all.extend(page.rows);
                if page.cursor.is_none() {
                    break;
                }
                query = query.after(page.cursor.as_deref())?;
            }
            read_back.insert(table.name.clone(), all);
        }
        Ok::<_, CliError>((kept, read_back))
    }
    .await;
    let _ = store.database().close().await;
    result
}

fn read_old_catalog_rows(path: &Path) -> Result<Vec<Value>, CliError> {
    // Synchronous open via a dedicated thread using SqliteDatabase helper via block_in_place
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let db = SqliteDatabase::open(path, SqliteOptions::default()).await?;
            let result = db
                .call(|conn| {
                    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
                    if !(1..=4).contains(&version) {
                        return Err(pi_rs_chord::Error::Invalid(format!(
                            "catalog.sqlite: unsupported schema version {version}"
                        )));
                    }
                    let mut cols = BTreeSet::new();
                    let mut info = conn.prepare("PRAGMA table_info(sessions)")?;
                    let mut col_rows = info.query([])?;
                    while let Some(r) = col_rows.next()? {
                        cols.insert(r.get::<_, String>(1)?);
                    }
                    let mut stmt = conn.prepare("SELECT * FROM sessions ORDER BY id")?;
                    let names: Vec<String> =
                        stmt.column_names().into_iter().map(str::to_owned).collect();
                    let mut rows = stmt.query([])?;
                    let mut out = Vec::new();
                    while let Some(row) = rows.next()? {
                        let cell = |col: &str| -> Option<SqliteValue> {
                            names
                                .iter()
                                .position(|n| n == col)
                                .and_then(|idx| row.get::<_, SqliteValue>(idx).ok())
                        };
                        let id = cell("id")
                            .and_then(|v| val_text(&v).map(str::to_owned))
                            .ok_or_else(|| {
                                pi_rs_chord::Error::Invalid("catalog.sqlite: missing id".into())
                            })?;
                        let cwd = cell("cwd")
                            .and_then(|v| val_text(&v).map(str::to_owned))
                            .ok_or_else(|| {
                                pi_rs_chord::Error::Invalid("catalog.sqlite: missing cwd".into())
                            })?;
                        let title = cell("title")
                            .and_then(|v| val_text(&v).map(str::to_owned))
                            .ok_or_else(|| {
                                pi_rs_chord::Error::Invalid("catalog.sqlite: missing title".into())
                            })?;
                        let env = if cols.contains("env") {
                            cell("env").and_then(|v| val_text(&v).map(str::to_owned))
                        } else {
                            None
                        };
                        let title_edited = if cols.contains("titleEdited") {
                            cell("titleEdited").and_then(|v| val_int(&v)) == Some(1)
                        } else {
                            title != "New session"
                        };
                        let created_at =
                            cell("createdAt").and_then(|v| val_int(&v)).ok_or_else(|| {
                                pi_rs_chord::Error::Invalid(
                                    "catalog.sqlite: missing createdAt".into(),
                                )
                            })?;
                        let updated_at =
                            cell("updatedAt").and_then(|v| val_int(&v)).ok_or_else(|| {
                                pi_rs_chord::Error::Invalid(
                                    "catalog.sqlite: missing updatedAt".into(),
                                )
                            })?;
                        let archived_at = cell("archivedAt").and_then(|v| val_int(&v));
                        let pinned = cell("pinned").and_then(|v| val_int(&v)) == Some(1);
                        let model_raw = cell("model")
                            .and_then(|v| val_text(&v).map(str::to_owned))
                            .ok_or_else(|| {
                                pi_rs_chord::Error::Invalid("catalog.sqlite: missing model".into())
                            })?;
                        let model: Value = serde_json::from_str(&model_raw)?;
                        let thinking_level = cell("thinkingLevel")
                            .and_then(|v| val_text(&v).map(str::to_owned))
                            .ok_or_else(|| {
                                pi_rs_chord::Error::Invalid(
                                    "catalog.sqlite: missing thinkingLevel".into(),
                                )
                            })?;
                        let main = if cols.contains("main") {
                            cell("main").and_then(|v| val_int(&v)).unwrap_or(1)
                        } else {
                            1
                        };
                        let branches: Value = if cols.contains("branches") {
                            let raw = cell("branches")
                                .and_then(|v| val_text(&v).map(str::to_owned))
                                .unwrap_or_else(|| "[]".into());
                            serde_json::from_str(&raw)?
                        } else {
                            json!([])
                        };
                        let mut obj = Map::new();
                        obj.insert("session".into(), session_address(&id));
                        obj.insert("cwd".into(), json!(cwd));
                        if let Some(env) = env {
                            obj.insert("env".into(), json!(env));
                        }
                        obj.insert("title".into(), json!(title));
                        obj.insert("titleEdited".into(), json!(title_edited));
                        obj.insert("createdAt".into(), json!(created_at));
                        obj.insert("updatedAt".into(), json!(updated_at));
                        if let Some(archived_at) = archived_at {
                            obj.insert("archivedAt".into(), json!(archived_at));
                        }
                        obj.insert("pinned".into(), json!(pinned));
                        obj.insert("model".into(), model);
                        obj.insert("thinkingLevel".into(), json!(thinking_level));
                        obj.insert("main".into(), json!(main));
                        obj.insert("branches".into(), branches);
                        out.push(Value::Object(obj));
                    }
                    Ok(out)
                })
                .await;
            let _ = db.close().await;
            Ok(result?)
        })
    })
}

async fn migrate_catalog(
    data_dir: &Path,
    work: &Path,
    dry_run: bool,
) -> Result<CatalogMigration, CliError> {
    let old = data_dir.join("catalog.sqlite");
    if !old.exists() {
        return Ok(CatalogMigration::Absent);
    }
    let server_dir = host_dirs::server_directory(work);
    if dry_run
        && !host_dirs::host_store(&server_dir).exists()
        && host_dirs::host_store(&host_dirs::server_directory(data_dir)).exists()
    {
        copy_sqlite_with_journal(
            &host_dirs::host_store(&host_dirs::server_directory(data_dir)),
            &host_dirs::host_store(&server_dir),
        )?;
    }
    let scratch = tempfile::tempdir()?;
    let copy = scratch.path().join("catalog.sqlite");
    copy_sqlite_with_journal(&old, &copy)?;
    let rows = read_old_catalog_rows(&copy)?;
    let count = rows.len();
    let catalog = CatalogTable::new()?;
    let history = catalog.history().clone();
    let (kept, _) = table_put_all(
        &server_dir,
        "p2.catalog",
        std::slice::from_ref(&history),
        &history.definition,
        vec![(&history.definition, rows.clone())],
    )
    .await?;
    if kept {
        if !dry_run {
            move_backup_files(
                data_dir,
                "pre-catalog-backup",
                &[PathBuf::from("catalog.sqlite")],
            )?;
        }
        return Ok(CatalogMigration::Done);
    }
    let (_, stored_map) = table_put_all(
        &server_dir,
        "p2.catalog",
        std::slice::from_ref(&history),
        &history.definition,
        vec![(&history.definition, Vec::new())],
    )
    .await?;
    let stored = stored_map
        .get(&history.definition.name)
        .cloned()
        .unwrap_or_default();
    let key = |row: &Value| {
        row["session"]["params"]["id"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    };
    let mut left: BTreeMap<String, String> = rows
        .iter()
        .map(|r| (key(r), serde_json::to_string(r).unwrap_or_default()))
        .collect();
    let mut right: BTreeMap<String, String> = stored
        .iter()
        .map(|r| {
            let cleaned = Value::Object(
                r.as_object()
                    .into_iter()
                    .flatten()
                    .filter(|(_, v)| !v.is_null())
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            );
            (
                key(&cleaned),
                serde_json::to_string(&cleaned).unwrap_or_default(),
            )
        })
        .collect();
    if left.len() != right.len()
        || left.keys().collect::<Vec<_>>() != right.keys().collect::<Vec<_>>()
    {
        left.clear();
        right.clear();
        let _ = (left, right);
        return Err("The catalog differs after the move".into());
    }
    if !dry_run {
        move_backup_files(
            data_dir,
            "pre-catalog-backup",
            &[PathBuf::from("catalog.sqlite")],
        )?;
    }
    Ok(CatalogMigration::Migrated { sessions: count })
}

async fn read_old_shares(path: &Path) -> Result<(Vec<Value>, Vec<Value>, Vec<Value>), CliError> {
    let db = SqliteDatabase::open(path, SqliteOptions::default()).await?;
    let result = db
        .call(|conn| {
            let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            if version != 1 && version != 2 {
                return Err(pi_rs_chord::Error::Invalid(format!(
                    "shares.sqlite: unsupported schema version {version}"
                )));
            }
            let check_mode = |m: &str| -> pi_rs_chord::Result<()> {
                match m {
                    "view" | "full" => Ok(()),
                    _ => Err(pi_rs_chord::Error::Invalid(format!(
                        "shares.sqlite: unknown mode {m}"
                    ))),
                }
            };
            let mut links = Vec::new();
            let mut session_of_link = BTreeMap::new();
            let mut mode_of_link = BTreeMap::new();
            {
                let mut stmt = conn.prepare("SELECT id, session, mode, token FROM links")?;
                let mut rows = stmt.query([])?;
                while let Some(r) = rows.next()? {
                    let id: String = r.get(0)?;
                    let session: String = r.get(1)?;
                    let mode: String = r.get(2)?;
                    let token: String = r.get(3)?;
                    check_mode(&mode)?;
                    session_of_link.insert(id.clone(), session.clone());
                    mode_of_link.insert(id.clone(), mode.clone());
                    links.push(json!({
                        "session": session_address(&session),
                        "mode": mode,
                        "id": id,
                        "token": token,
                    }));
                }
            }
            let mut guests = Vec::new();
            let mut grants = Vec::new();
            if version == 1 {
                let mut stmt = conn.prepare(
                    "SELECT id, linkId, name, secretHash, removed, revision, lastSeen FROM guests",
                )?;
                let mut rows = stmt.query([])?;
                while let Some(r) = rows.next()? {
                    let id: String = r.get(0)?;
                    let link_id: String = r.get(1)?;
                    let name: String = r.get(2)?;
                    let secret_hash: String = r.get(3)?;
                    let removed: i64 = r.get(4)?;
                    let revision: i64 = r.get(5)?;
                    let last_seen: i64 = r.get(6)?;
                    guests.push(json!({
                        "id": id,
                        "name": name,
                        "secretHash": secret_hash,
                        "revision": revision,
                        "lastSeen": last_seen,
                    }));
                    if let Some(session) = session_of_link.get(&link_id) {
                        grants.push(json!({
                            "session": session_address(session),
                            "guest": id,
                            "mode": mode_of_link.get(&link_id).cloned().unwrap_or_else(|| "view".into()),
                            "linkId": link_id,
                            "removed": removed == 1,
                            "grantedAt": last_seen,
                        }));
                    }
                }
            } else {
                {
                    let mut stmt = conn.prepare(
                        "SELECT id, name, secretHash, email, revision, lastSeen FROM guests",
                    )?;
                    let mut rows = stmt.query([])?;
                    while let Some(r) = rows.next()? {
                        let id: String = r.get(0)?;
                        let name: String = r.get(1)?;
                        let secret_hash: Option<String> = r.get(2)?;
                        let email: Option<String> = r.get(3)?;
                        let revision: i64 = r.get(4)?;
                        let last_seen: i64 = r.get(5)?;
                        let mut obj = Map::new();
                        obj.insert("id".into(), json!(id));
                        obj.insert("name".into(), json!(name));
                        if let Some(h) = secret_hash {
                            obj.insert("secretHash".into(), json!(h));
                        }
                        if let Some(e) = email {
                            obj.insert("email".into(), json!(e));
                        }
                        obj.insert("revision".into(), json!(revision));
                        obj.insert("lastSeen".into(), json!(last_seen));
                        guests.push(Value::Object(obj));
                    }
                }
                {
                    let mut stmt = conn.prepare(
                        "SELECT guest, session, mode, linkId, removed, grantedAt FROM grants",
                    )?;
                    let mut rows = stmt.query([])?;
                    while let Some(r) = rows.next()? {
                        let guest: String = r.get(0)?;
                        let session: String = r.get(1)?;
                        let mode: String = r.get(2)?;
                        let link_id: Option<String> = r.get(3)?;
                        let removed: i64 = r.get(4)?;
                        let granted_at: i64 = r.get(5)?;
                        check_mode(&mode)?;
                        let mut obj = Map::new();
                        obj.insert("session".into(), session_address(&session));
                        obj.insert("guest".into(), json!(guest));
                        obj.insert("mode".into(), json!(mode));
                        if let Some(l) = link_id {
                            obj.insert("linkId".into(), json!(l));
                        }
                        obj.insert("removed".into(), json!(removed == 1));
                        obj.insert("grantedAt".into(), json!(granted_at));
                        grants.push(Value::Object(obj));
                    }
                }
            }
            Ok((links, guests, grants))
        })
        .await;
    let _ = db.close().await;
    Ok(result?)
}

async fn read_old_children(
    path: &Path,
    envs: &BTreeMap<String, Option<String>>,
) -> Result<Vec<Value>, CliError> {
    let db = SqliteDatabase::open(path, SqliteOptions::default()).await?;
    let envs = envs.clone();
    let result = db
        .call(move |conn| {
            let mut stmt = conn.prepare("SELECT session, value FROM children")?;
            let mut rows = stmt.query([])?;
            let mut out = Vec::new();
            while let Some(r) = rows.next()? {
                let session: String = r.get(0)?;
                let raw: String = r.get(1)?;
                let parsed: Vec<Value> = serde_json::from_str(&raw)?;
                let session_env = envs.get(&session).cloned().flatten();
                let placed: Vec<Value> = parsed
                    .into_iter()
                    .map(|mut child| {
                        if let Some(obj) = child.as_object_mut()
                            && obj.get("location").is_none()
                            && let Some(cwd) = obj.remove("cwd")
                        {
                            obj.insert(
                                "location".into(),
                                json!({ "cwd": cwd, "env": session_env }),
                            );
                        }
                        child
                    })
                    .collect();
                out.push(json!({
                    "session": session_address(&session),
                    "children": placed,
                }));
            }
            Ok(out)
        })
        .await;
    let _ = db.close().await;
    Ok(result?)
}

async fn migrate_server_data(
    data_dir: &Path,
    work: &Path,
    dry_run: bool,
) -> Result<ServerDataMigration, CliError> {
    let server_dir = host_dirs::server_directory(work);
    if dry_run
        && !host_dirs::host_store(&server_dir).exists()
        && host_dirs::host_store(&host_dirs::server_directory(data_dir)).exists()
    {
        copy_sqlite_with_journal(
            &host_dirs::host_store(&host_dirs::server_directory(data_dir)),
            &host_dirs::host_store(&server_dir),
        )?;
    }

    let shares = {
        let old = data_dir.join("shares.sqlite");
        if !old.exists() {
            FileMigration::Absent
        } else {
            let scratch = tempfile::tempdir()?;
            let copy = scratch.path().join("shares.sqlite");
            copy_sqlite_with_journal(&old, &copy)?;
            let (links, guests, grants) = read_old_shares(&copy).await?;
            let row_count = links.len() + guests.len() + grants.len();
            if row_count > 0 {
                let tables = ShareTablesDef::new()?;
                let histories = vec![
                    TableHistory::base(tables.links.clone()),
                    TableHistory::base(tables.guests.clone()),
                    TableHistory::base(tables.grants.clone()),
                ];
                let (kept, _) = table_put_all(
                    &server_dir,
                    "p2.shares",
                    &histories,
                    &tables.links,
                    vec![
                        (&tables.links, links.clone()),
                        (&tables.guests, guests.clone()),
                        (&tables.grants, grants.clone()),
                    ],
                )
                .await?;
                if kept {
                    if !dry_run {
                        move_backup_files(
                            data_dir,
                            "pre-server-data-backup",
                            &[PathBuf::from("shares.sqlite")],
                        )?;
                    }
                    FileMigration::Done
                } else {
                    let (_, stored) = table_put_all(
                        &server_dir,
                        "p2.shares",
                        &histories,
                        &tables.links,
                        vec![
                            (&tables.links, Vec::new()),
                            (&tables.guests, Vec::new()),
                            (&tables.grants, Vec::new()),
                        ],
                    )
                    .await?;
                    if stored.get("links").map(Vec::len).unwrap_or_default() != links.len()
                        || stored.get("guests").map(Vec::len).unwrap_or_default() != guests.len()
                        || stored.get("grants").map(Vec::len).unwrap_or_default() != grants.len()
                    {
                        return Err("shares.sqlite differs after the move".into());
                    }
                    if !dry_run {
                        move_backup_files(
                            data_dir,
                            "pre-server-data-backup",
                            &[PathBuf::from("shares.sqlite")],
                        )?;
                    }
                    FileMigration::Migrated { rows: row_count }
                }
            } else {
                if !dry_run {
                    move_backup_files(
                        data_dir,
                        "pre-server-data-backup",
                        &[PathBuf::from("shares.sqlite")],
                    )?;
                }
                FileMigration::Migrated { rows: 0 }
            }
        }
    };

    let subagents = {
        let old = data_dir.join("subagents.sqlite");
        if !old.exists() {
            FileMigration::Absent
        } else {
            let scratch = tempfile::tempdir()?;
            let copy = scratch.path().join("subagents.sqlite");
            copy_sqlite_with_journal(&old, &copy)?;
            let mut envs = BTreeMap::new();
            for (id, _, _, env, _, _) in catalog_rows(work).await? {
                envs.insert(id, env);
            }
            let rows = read_old_children(&copy, &envs).await?;
            let row_count = rows.len();
            if row_count > 0 {
                let table = child_rows_table()?;
                let history = TableHistory::base(table.clone());
                let (kept, _) = table_put_all(
                    &server_dir,
                    "p2.child-store",
                    std::slice::from_ref(&history),
                    &table,
                    vec![(&table, rows.clone())],
                )
                .await?;
                if kept {
                    if !dry_run {
                        move_backup_files(
                            data_dir,
                            "pre-server-data-backup",
                            &[PathBuf::from("subagents.sqlite")],
                        )?;
                    }
                    FileMigration::Done
                } else {
                    let (_, stored) = table_put_all(
                        &server_dir,
                        "p2.child-store",
                        std::slice::from_ref(&history),
                        &table,
                        vec![(&table, Vec::new())],
                    )
                    .await?;
                    if stored.get("children").map(Vec::len).unwrap_or_default() != row_count {
                        return Err("subagents.sqlite differs after the move".into());
                    }
                    if !dry_run {
                        move_backup_files(
                            data_dir,
                            "pre-server-data-backup",
                            &[PathBuf::from("subagents.sqlite")],
                        )?;
                    }
                    FileMigration::Migrated { rows: row_count }
                }
            } else {
                if !dry_run {
                    move_backup_files(
                        data_dir,
                        "pre-server-data-backup",
                        &[PathBuf::from("subagents.sqlite")],
                    )?;
                }
                FileMigration::Migrated { rows: 0 }
            }
        }
    };

    Ok(ServerDataMigration { shares, subagents })
}

struct DurableMigrationCounts {
    entries: usize,
    documents: usize,
}

async fn convert_durable(
    old_path: &Path,
    store: &StoreEngine,
    storage: &dyn Storage,
) -> Result<(BTreeMap<i64, i64>, DurableMigrationCounts), CliError> {
    let db = SqliteDatabase::open(
        old_path,
        SqliteOptions {
            mode: Mode::ReadOnly,
            ..Default::default()
        },
    )
    .await?;

    let snapshot = db
        .call(|conn| {
            let mut conv_records = Vec::new();
            {
                let mut stmt = conn.prepare("SELECT record FROM conversations ORDER BY id")?;
                let mut rows = stmt.query([])?;
                while let Some(r) = rows.next()? {
                    let s: String = r.get(0)?;
                    conv_records.push(serde_json::from_str::<pi_rs_durable::ConversationRecord>(&s)?);
                }
            }
            let mut task_records = Vec::new();
            {
                let mut stmt = conn.prepare("SELECT record FROM tasks ORDER BY id")?;
                let mut rows = stmt.query([])?;
                while let Some(r) = rows.next()? {
                    let s: String = r.get(0)?;
                    task_records.push(serde_json::from_str::<TaskRecord>(&s)?);
                }
            }
            let mut submission_records = Vec::new();
            {
                let mut stmt = conn.prepare("SELECT record FROM submissions ORDER BY id")?;
                let mut rows = stmt.query([])?;
                while let Some(r) = rows.next()? {
                    let s: String = r.get(0)?;
                    submission_records.push(serde_json::from_str::<SubmissionRecord>(&s)?);
                }
            }
            let mut entries = Vec::new();
            {
                let mut stmt = conn.prepare("SELECT commit_seq, record FROM entries ORDER BY id")?;
                let mut rows = stmt.query([])?;
                while let Some(r) = rows.next()? {
                    let seq: i64 = r.get(0)?;
                    let s: String = r.get(1)?;
                    entries.push((seq, serde_json::from_str::<EntryRecord>(&s)?));
                }
            }
            let mut revisions: BTreeMap<u64, Vec<(i64, String, u64, String)>> = BTreeMap::new();
            {
                let mut stmt = conn.prepare(
                    "SELECT document_id, seq, kind, version, content FROM document_revisions ORDER BY document_id, seq",
                )?;
                let mut rows = stmt.query([])?;
                while let Some(r) = rows.next()? {
                    let doc_id: i64 = r.get(0)?;
                    let seq: i64 = r.get(1)?;
                    let kind: String = r.get(2)?;
                    let version: i64 = r.get(3)?;
                    let content: String = r.get(4)?;
                    revisions
                        .entry(u64::try_from(doc_id).unwrap_or_default())
                        .or_default()
                        .push((seq, kind, u64::try_from(version).unwrap_or(1), content));
                }
            }
            let mut documents = Vec::new();
            {
                let mut stmt = conn.prepare(
                    "SELECT id, created_at, retired_at, record FROM documents ORDER BY id",
                )?;
                let mut rows = stmt.query([])?;
                while let Some(r) = rows.next()? {
                    let id: i64 = r.get(0)?;
                    let created_at: i64 = r.get(1)?;
                    let retired_at: Option<i64> = r.get(2)?;
                    let record: String = r.get(3)?;
                    documents.push((
                        u64::try_from(id).unwrap_or_default(),
                        created_at,
                        retired_at,
                        serde_json::from_str::<DocumentRecord>(&record)?,
                    ));
                }
            }
            let next_id: i64 = conn
                .query_row(
                    "SELECT next_id FROM durable_metadata WHERE singleton = 1",
                    [],
                    |r| r.get::<_, String>(0),
                )
                .ok()
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(2);
            Ok((
                conv_records,
                task_records,
                submission_records,
                entries,
                revisions,
                documents,
                next_id,
            ))
        })
        .await;
    let _ = db.close().await;
    let (conv_records, task_records, submission_records, entries, revisions, documents, next_id) =
        snapshot?;

    store
        .database()
        .run(
            "UPDATE chord_meta SET next_id = max(next_id, ?) WHERE singleton = 1",
            vec![SqliteValue::Integer(next_id)],
        )
        .await?;

    let mut first = Vec::new();
    for value in conv_records {
        first.push(StorageWrite::Conversation { value });
    }
    for value in task_records {
        first.push(StorageWrite::Task { value });
    }
    for value in submission_records {
        first.push(StorageWrite::Submission { value });
    }

    let entry_count = entries.len();
    let doc_count = documents.len();
    let mut by_seq: BTreeMap<i64, Vec<StorageWrite>> = BTreeMap::new();
    for (seq, entry) in entries {
        by_seq
            .entry(seq)
            .or_default()
            .push(StorageWrite::Entry { value: entry });
    }

    for (id, created_at, retired_at, doc) in documents {
        let revs = revisions.get(&id).map(Vec::as_slice).unwrap_or(&[]);
        let (first_rev, later_revs) = revs
            .split_first()
            .map(|(f, l)| (Some(f), l))
            .unwrap_or((None, &[]));
        let base_content = match first_rev {
            Some((_, kind, version, content)) => {
                if kind != "base" {
                    return Err(format!("Document {id}'s earliest revision is no base").into());
                }
                DocumentContent::Base {
                    version: *version,
                    value: serde_json::from_str(content)?,
                }
            }
            None => {
                if retired_at.is_none() {
                    return Err(format!("Live document {id} has no content").into());
                }
                DocumentContent::Base {
                    version: 1,
                    value: Map::new(),
                }
            }
        };
        let create = DocumentCreate {
            id: doc.id,
            scope: doc.scope,
            history: doc.history,
            kind: doc.kind,
            key: doc.key,
            fork: doc.fork,
        };
        by_seq
            .entry(created_at)
            .or_default()
            .push(StorageWrite::DocumentCreate {
                record: create,
                content: base_content,
            });
        for (rev_seq, kind, version, content_raw) in later_revs {
            let content = if kind == "base" {
                DocumentContent::Base {
                    version: *version,
                    value: serde_json::from_str(content_raw)?,
                }
            } else {
                let val: Value = serde_json::from_str(content_raw)?;
                serde_json::from_value(json!({
                    "kind": "delta",
                    "version": *version,
                    "ops": val,
                }))?
            };
            by_seq
                .entry(*rev_seq)
                .or_default()
                .push(StorageWrite::DocumentChange {
                    id: doc.id,
                    content,
                });
        }
        if let Some(retired) = retired_at {
            by_seq
                .entry(retired)
                .or_default()
                .push(StorageWrite::DocumentRetire { id: doc.id });
        }
    }

    let mut batches: Vec<(Option<i64>, Vec<StorageWrite>)> = vec![(None, first)];
    for (old_seq, writes) in by_seq {
        batches.push((Some(old_seq), writes));
    }

    let mut seqs = BTreeMap::new();
    for chunk in batches.chunks(MIGRATION_COMMITS_PER_CALL) {
        let write_batches: Vec<Vec<StorageWrite>> = chunk.iter().map(|(_, w)| w.clone()).collect();
        let mut assigned = Vec::new();
        let res = storage.commit(&write_batches, &mut |idx, new_seq| {
            assigned.push((idx, new_seq));
        })?;
        match res {
            CommitResult::Stored { stored } if stored == chunk.len() => {
                for (idx, new_seq) in assigned {
                    if let Some(old_seq) = chunk[idx].0 {
                        seqs.insert(old_seq, i64::try_from(new_seq).unwrap_or_default());
                    }
                }
            }
            other => {
                return Err(format!("The store refused a replayed commit: {other:?}").into());
            }
        }
    }

    Ok((
        seqs,
        DurableMigrationCounts {
            entries: entry_count,
            documents: doc_count,
        },
    ))
}

async fn verify_durable(before: &dyn Storage, after: &dyn Storage) -> Result<(), CliError> {
    let mut convs_before = Vec::new();
    let mut cursor = None;
    loop {
        let page = before.scan_conversations(
            &pi_rs_durable::ConversationQuery::default(),
            500,
            cursor.as_ref(),
        )?;
        convs_before.extend(page.items);
        if page.next.is_none() {
            break;
        }
        cursor = page.next;
    }
    let mut convs_after = Vec::new();
    let mut cursor = None;
    loop {
        let page = after.scan_conversations(
            &pi_rs_durable::ConversationQuery::default(),
            500,
            cursor.as_ref(),
        )?;
        convs_after.extend(page.items);
        if page.next.is_none() {
            break;
        }
        cursor = page.next;
    }
    if serde_json::to_value(&convs_before)? != serde_json::to_value(&convs_after)? {
        return Err("conversations differ after migration".into());
    }

    for conv in &convs_before {
        let q = EntryQuery {
            conversation_id: conv.id,
            order: Some(ScanOrder::Ascending),
            ..Default::default()
        };
        let mut eb = Vec::new();
        let mut cur = None;
        loop {
            let page = before.scan_entries(&q, 500, cur.as_ref())?;
            eb.extend(page.items);
            if page.next.is_none() {
                break;
            }
            cur = page.next;
        }
        let mut ea = Vec::new();
        let mut cur = None;
        loop {
            let page = after.scan_entries(&q, 500, cur.as_ref())?;
            ea.extend(page.items);
            if page.next.is_none() {
                break;
            }
            cur = page.next;
        }
        if serde_json::to_value(&eb)? != serde_json::to_value(&ea)? {
            return Err(format!("entries of {} differ after conversion", conv.id).into());
        }
    }

    let mut tb = Vec::new();
    let mut cur = None;
    loop {
        let page = before.scan_tasks(&TaskQuery::default(), 500, cur.as_ref())?;
        tb.extend(page.items);
        if page.next.is_none() {
            break;
        }
        cur = page.next;
    }
    let mut ta = Vec::new();
    let mut cur = None;
    loop {
        let page = after.scan_tasks(&TaskQuery::default(), 500, cur.as_ref())?;
        ta.extend(page.items);
        if page.next.is_none() {
            break;
        }
        cur = page.next;
    }
    if serde_json::to_value(&tb)? != serde_json::to_value(&ta)? {
        return Err("tasks differ after conversion".into());
    }

    let mut sb = Vec::new();
    let mut cur = None;
    loop {
        let page = before.scan_submissions(&SubmissionQuery::default(), 500, cur.as_ref())?;
        sb.extend(page.items);
        if page.next.is_none() {
            break;
        }
        cur = page.next;
    }
    let mut sa = Vec::new();
    let mut cur = None;
    loop {
        let page = after.scan_submissions(&SubmissionQuery::default(), 500, cur.as_ref())?;
        sa.extend(page.items);
        if page.next.is_none() {
            break;
        }
        cur = page.next;
    }
    if serde_json::to_value(&sb)? != serde_json::to_value(&sa)? {
        return Err("submissions differ after conversion".into());
    }

    let mut scopes = vec![DocumentScope::Session];
    for conv in &convs_before {
        scopes.push(DocumentScope::Conversation {
            conversation_id: conv.id,
        });
    }
    for task in &tb {
        scopes.push(DocumentScope::Task { task_id: task.id });
    }
    for scope in scopes {
        let q = DocumentQuery {
            scope,
            at: DocumentPoint::Current,
            kind: None,
        };
        let mut db_docs = Vec::new();
        let mut cur = None;
        loop {
            let page = before.scan_documents(&q, 500, cur.as_ref())?;
            db_docs.extend(page.items);
            if page.next.is_none() {
                break;
            }
            cur = page.next;
        }
        let mut da_docs = Vec::new();
        let mut cur = None;
        loop {
            let page = after.scan_documents(&q, 500, cur.as_ref())?;
            da_docs.extend(page.items);
            if page.next.is_none() {
                break;
            }
            cur = page.next;
        }
        if db_docs.len() != da_docs.len() {
            return Err("document count differs after conversion".into());
        }
        for doc in db_docs {
            let a = before
                .document(doc.id, DocumentPoint::Current)?
                .ok_or("Missing document before migration")?;
            let b = after
                .document(doc.id, DocumentPoint::Current)?
                .ok_or("Missing document after migration")?;
            if a.version != b.version || a.value != b.value {
                return Err(format!("document {} differs after conversion", doc.id).into());
            }
            if matches!(doc.scope, DocumentScope::Conversation { .. })
                && matches!(doc.history, Some(DocumentHistory::Rewindable))
                && before
                    .find_document(
                        &DocumentAddress {
                            scope: doc.scope,
                            kind: doc.kind.clone(),
                            key: doc.key.clone(),
                        },
                        DocumentPoint::Current,
                    )?
                    .is_some()
                && after
                    .find_document(
                        &DocumentAddress {
                            scope: doc.scope,
                            kind: doc.kind,
                            key: doc.key,
                        },
                        DocumentPoint::Current,
                    )?
                    .is_none()
            {
                return Err(format!("rewindable document {} lookup failed", doc.id).into());
            }
        }
    }
    Ok(())
}

struct LegacyFileEntry {
    space: String,
    path: String,
    key: Option<String>,
    mime: String,
    source: String,
    meta: Value,
    bytes: Vec<u8>,
    version: Option<u64>,
    created: u64,
}

fn modified_ms(path: &Path) -> u64 {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

fn collect_old_files(data_dir: &Path, id: &str) -> Result<Vec<LegacyFileEntry>, CliError> {
    let uuid_re = Regex::new(
        r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$",
    )?;
    let mut files = Vec::new();
    let artifacts_dir = data_dir.join("artifacts").join(id);
    for name in dir_entries(&artifacts_dir) {
        let mut versions: Vec<u64> = dir_entries(&artifacts_dir.join(&name))
            .into_iter()
            .filter_map(|v| v.parse::<u64>().ok())
            .filter(|&v| v > 0)
            .collect();
        versions.sort_unstable();
        for version in versions {
            let vpath = artifacts_dir
                .join(&name)
                .join(version.to_string())
                .join("version.json");
            if !vpath.exists() {
                continue;
            }
            let parsed: Value = serde_json::from_slice(&fs::read(&vpath)?)?;
            let info_name = parsed["info"]["name"].as_str().unwrap_or_default();
            let info_version = parsed["info"]["version"].as_u64().unwrap_or_default();
            if info_name != name || info_version != version {
                return Err(format!("Artifact {name} version {version} names another").into());
            }
            let artifact_type = parsed["info"]["type"].as_str().unwrap_or("text");
            let title = parsed["info"]["title"].as_str().unwrap_or_default();
            let content = parsed["content"].as_str().unwrap_or_default();
            let mime = match artifact_type {
                "html" => "text/html",
                "svg" => "image/svg+xml",
                "markdown" => "text/markdown",
                _ => "text/plain",
            };
            files.push(LegacyFileEntry {
                space: "artifacts".into(),
                path: name.clone(),
                key: None,
                mime: mime.into(),
                source: "agent".into(),
                meta: json!({ "title": title, "type": artifact_type }),
                bytes: content.as_bytes().to_vec(),
                version: Some(version),
                created: modified_ms(&vpath),
            });
        }
    }

    let uploads_dir = data_dir.join("uploads").join(id);
    for file in dir_entries(&uploads_dir) {
        if file.len() < 37 || file.as_bytes().get(36) != Some(&b'-') {
            continue;
        }
        let key = &file[..36];
        let full = uploads_dir.join(&file);
        if !uuid_re.is_match(key) || !full.is_file() {
            continue;
        }
        let display_name = &file[37..];
        let ext = display_name
            .rsplit('.')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let mime = match ext.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            _ => "application/octet-stream",
        };
        files.push(LegacyFileEntry {
            space: "uploads".into(),
            path: file.clone(),
            key: Some(key.to_owned()),
            mime: mime.into(),
            source: "unknown".into(),
            meta: json!({ "name": display_name }),
            bytes: fs::read(&full)?,
            version: None,
            created: modified_ms(&full),
        });
    }

    let downloads_dir = data_dir.join("downloads").join(id);
    for key in dir_entries(&downloads_dir) {
        let meta_path = downloads_dir.join(&key).join("metadata.json");
        if !uuid_re.is_match(&key) || !meta_path.exists() {
            continue;
        }
        let info: Value = serde_json::from_slice(&fs::read(&meta_path)?)?;
        if info["id"].as_str() != Some(key.as_str()) {
            return Err(format!("Download {key} names another").into());
        }
        let name = info["name"].as_str().ok_or("Download missing name")?;
        files.push(LegacyFileEntry {
            space: "downloads".into(),
            path: format!("{key}-{name}"),
            key: Some(key.clone()),
            mime: "application/octet-stream".into(),
            source: "agent".into(),
            meta: json!({ "name": name }),
            bytes: fs::read(downloads_dir.join(&key).join("payload"))?,
            version: None,
            created: modified_ms(&meta_path),
        });
    }
    Ok(files)
}

async fn put_file_bytes(
    files: &Files,
    write: FileWrite,
    bytes: &[u8],
) -> Result<pi_rs_chord::node::app::FileInfo, CliError> {
    if bytes.len() <= FILES_CHUNK_BYTES {
        return Ok(files.put(write, bytes).await?);
    }
    let id = files.begin(write)?;
    let mut offset = 0usize;
    while offset < bytes.len() {
        let end = (offset + FILES_CHUNK_BYTES).min(bytes.len());
        files
            .append(&id, offset as u64, &bytes[offset..end])
            .await?;
        offset = end;
    }
    Ok(files.finish(&id).await?)
}

async fn migrate_and_verify_files(
    data_dir: &Path,
    staging: &Path,
    store: StoreEngine,
    id: &str,
) -> Result<(usize, u64), CliError> {
    let legacy = collect_old_files(data_dir, id)?;
    let files = Files::open(store, host_dirs::host_files(staging), FilesCaps::default()).await?;
    let mut holes = Vec::new();
    let mut file_count = 0usize;
    let mut total_bytes = 0u64;
    for old in &legacy {
        if let Some(target_version) = old.version {
            let current = files
                .get(&old.space, &old.path, None)?
                .map(|f| f.version)
                .unwrap_or(0);
            for next in (current + 1)..target_version {
                let mut hole = FileWrite::new(&old.space, &old.path);
                hole.mime = "text/plain".into();
                hole.source = "migration".into();
                files.put(hole, &[]).await?;
                holes.push((old.space.clone(), old.path.clone(), next));
            }
        }
        let mut write = FileWrite::new(&old.space, &old.path);
        write.mime = old.mime.clone();
        write.source = old.source.clone();
        write.meta = Some(old.meta.clone());
        write.key = old.key.clone();
        write.created = Some(old.created);
        let stored = put_file_bytes(&files, write, &old.bytes).await?;
        if let Some(expected_version) = old.version
            && stored.version != expected_version
        {
            return Err(format!(
                "Artifact {} version {} became version {}",
                old.path, expected_version, stored.version
            )
            .into());
        }
        total_bytes += stored.size;
        file_count += 1;
    }
    for (space, path, version) in holes {
        files.remove(&space, &path, Some(version)).await?;
    }

    for old in &legacy {
        let found = match &old.key {
            Some(key) => files.by_key(&old.space, key),
            None => files.get(&old.space, &old.path, old.version)?,
        }
        .ok_or_else(|| format!("{}:{} missing after migration", old.space, old.path))?;
        let read_len = usize::try_from(found.size)?;
        let actual = files.read(&found.id, 0, read_len)?;
        if actual != old.bytes {
            return Err(format!("{}:{} bytes differ after migration", old.space, old.path).into());
        }
    }
    Ok((file_count, total_bytes))
}

async fn migrate_session(
    data_dir: &Path,
    work: &Path,
    id: &str,
    dry_run: bool,
) -> Result<Option<(usize, usize, usize, u64)>, CliError> {
    let old = data_dir.join("sessions").join(format!("{id}.sqlite"));
    let target = host_dirs::session_directory(work, id)?;
    let backup_paths = vec![
        PathBuf::from("sessions").join(format!("{id}.sqlite")),
        PathBuf::from("artifacts").join(id),
        PathBuf::from("downloads").join(id),
    ];
    if !dry_run && target.exists() {
        move_backup_files(data_dir, "pre-files-backup", &backup_paths)?;
        return Ok(None);
    }
    let staging = if dry_run {
        target.clone()
    } else {
        data_dir.join("sessions").join(format!(".{id}.migrating"))
    };
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    let scratch = staging.join(".old");
    private_directory(&scratch)?;
    let copy = scratch.join("old.sqlite");
    copy_sqlite_with_journal(&old, &copy)?;

    let (entries, documents, files, bytes) = {
        let store = host_dirs::open_host_store(&staging).await?;
        let res = async {
            let storage = durable_storage(&store)?;
            let (_, counts) = convert_durable(&copy, &store, &storage).await?;
            storage.close()?;
            let (file_count, file_bytes) =
                migrate_and_verify_files(data_dir, &staging, store.clone(), id).await?;
            Ok::<_, CliError>((counts.entries, counts.documents, file_count, file_bytes))
        }
        .await;
        let _ = store.database().close().await;
        res?
    };

    {
        let reopened = host_dirs::open_host_store(&staging).await?;
        let res = async {
            let before = SqliteStorage::open(&copy)?;
            let after = durable_storage(&reopened)?;
            let verify_res = verify_durable(&before, &after).await;
            let _ = after.close();
            let _ = before.close();
            verify_res
        }
        .await;
        let _ = reopened.database().close().await;
        res?;
    }

    fs::remove_dir_all(&scratch)?;
    if !dry_run {
        if let Some(parent) = target.parent() {
            private_directory(parent)?;
        }
        fs::rename(&staging, &target)?;
        move_backup_files(data_dir, "pre-files-backup", &backup_paths)?;
    }
    Ok(Some((entries, documents, files, bytes)))
}

pub async fn migrate_data(data_dir: &Path, dry_run: bool) -> Result<MigrationSummary, CliError> {
    let _lock = ProcessLock::acquire(&data_dir.join("server.lock"), Duration::from_secs(1))
        .await
        .map_err(|_| format!("A p2 server runs on {}: stop it first", data_dir.display()))?;

    let temp_work = if dry_run {
        Some(tempfile::tempdir()?)
    } else {
        None
    };
    let work = temp_work.as_ref().map(|d| d.path()).unwrap_or(data_dir);

    let server = migrate_server_store(data_dir, work, dry_run).await?;
    let catalog = migrate_catalog(data_dir, work, dry_run).await?;
    let server_data = migrate_server_data(data_dir, work, dry_run).await?;

    let mut session_ids = Vec::new();
    let sessions_dir = data_dir.join("sessions");
    if sessions_dir.exists() {
        for name in dir_entries(&sessions_dir) {
            if let Some(id) = name.strip_suffix(".sqlite") {
                session_ids.push(id.to_owned());
            }
        }
    }
    session_ids.sort();

    let mut migrated = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = Vec::new();
    let mut entries = 0usize;
    let mut documents = 0usize;
    let mut files = 0usize;
    let mut bytes = 0u64;

    for id in session_ids {
        match migrate_session(data_dir, work, &id, dry_run).await {
            Ok(None) => skipped.push(id),
            Ok(Some((e, d, f, b))) => {
                migrated.push(id);
                entries += e;
                documents += d;
                files += f;
                bytes += b;
            }
            Err(error) => failed.push(SessionMigrationError {
                id,
                error: error.to_string(),
            }),
        }
    }

    Ok(MigrationSummary {
        data_dir: data_dir.display().to_string(),
        dry_run,
        server,
        catalog,
        server_data,
        sessions: SessionsMigrationSummary {
            migrated,
            skipped,
            failed,
        },
        entries,
        documents,
        files,
        bytes,
    })
}

pub async fn migrate_data_command(args: &[String]) -> Result<(bool, String), CliError> {
    let data_dir = match option_value(args, "--data")? {
        Some(dir) => PathBuf::from(dir),
        None => data_directory()?,
    };
    let dry_run = args.iter().any(|arg| arg == "--dry-run");
    let summary = migrate_data(&data_dir, dry_run).await?;
    let ok = summary.sessions.failed.is_empty();
    Ok((ok, serde_json::to_string_pretty(&summary)?))
}

fn chosen_sessions(report: &DataReport) -> Vec<String> {
    let mut active: Vec<(&str, &SessionReport)> = report
        .sessions
        .iter()
        .filter(|(_, s)| !s.archived)
        .map(|(id, s)| (id.as_str(), s))
        .collect();
    active.sort_by_key(|a| std::cmp::Reverse(a.1.updated_at));
    let mut picked = Vec::new();
    let mut seen = BTreeSet::new();
    for (id, _) in active.iter().take(VERIFY_NEWEST_LIMIT) {
        if seen.insert((*id).to_owned()) {
            picked.push((*id).to_owned());
        }
    }
    for (id, s) in active {
        if (!s.subagents.is_empty() || !s.shares.links.is_empty()) && seen.insert(id.to_owned()) {
            picked.push(id.to_owned());
        }
    }
    picked
}

async fn local_owner_cookie(origin: &Url, data_dir: &Path) -> Result<String, CliError> {
    let port = origin
        .port_or_known_default()
        .ok_or("Missing port for local sign-in")?;
    let owner_key = fs::read_to_string(data_dir.join("owner-key"))
        .map_err(|_| "Missing owner key; start p2 first")?;
    let origin_header = origin.origin().ascii_serialization();
    let client = Client::builder().timeout(Duration::from_secs(10)).build()?;
    let challenge_res = client
        .post(origin.join(LOCAL_CHALLENGE_PATH)?)
        .header(header::ORIGIN, &origin_header)
        .send()
        .await?;
    if !challenge_res.status().is_success() {
        return Err(format!("Local sign-in unavailable ({})", challenge_res.status()).into());
    }
    let challenge_json: Value = challenge_res.json().await?;
    let challenge = challenge_json["challenge"]
        .as_str()
        .ok_or("Missing challenge")?;
    let proof = local_owner_proof(owner_key.trim(), port, challenge);
    let claim_res = client
        .post(origin.join(LOCAL_CLAIM_PATH)?)
        .header(header::ORIGIN, &origin_header)
        .json(&json!({ "challenge": challenge, "proof": proof }))
        .send()
        .await?;
    if !claim_res.status().is_success() {
        return Err(format!("Local sign-in claim failed ({})", claim_res.status()).into());
    }
    extract_cookie(&claim_res, &format!("p2_owner_{port}="))
        .ok_or_else(|| "Missing owner cookie".into())
}

pub async fn verify_sessions(
    url: &str,
    report: &DataReport,
    data_dir: &Path,
) -> Result<Vec<String>, CliError> {
    let origin = Url::parse(url)?;
    let host = origin.host_str().unwrap_or_default();
    if !matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1") {
        return Err("Use a loopback URL (the owner)".into());
    }
    let cookie = local_owner_cookie(&origin, data_dir).await?;
    let mut ws_url = origin.join("/chord?client=script")?;
    ws_url
        .set_scheme(if origin.scheme() == "https" {
            "wss"
        } else {
            "ws"
        })
        .map_err(|_| "Invalid WebSocket URL")?;

    let mut headers = header::HeaderMap::new();
    headers.insert(
        header::ORIGIN,
        origin.origin().ascii_serialization().parse()?,
    );
    let mut cookie_val = header::HeaderValue::from_str(&cookie)?;
    cookie_val.set_sensitive(true);
    headers.insert(header::COOKIE, cookie_val);

    let link =
        connect_websocket(ws_url.as_str(), headers, 4 * 1024 * 1024, 4 * 1024 * 1024).await?;
    let peer = RemotePeer::connect(
        link,
        RemoteOptions::new(json!({ "kind": "server" })),
        Batch::new(|_| {}),
    )?;
    let ctx = Context::background();

    let sessions_contract = std::sync::Arc::new(lookup("p2.sessions")?);
    let status_contract = std::sync::Arc::new(lookup("p2.session-status")?);
    let control_contract = std::sync::Arc::new(lookup("p2.worker-control")?);
    let chat_contract = std::sync::Arc::new(lookup("p2.chat")?);

    let sessions_held = peer.hold(sessions_contract, json!({}).into(), &ctx)?;
    let status_held = peer.hold(status_contract, json!({}).into(), &ctx)?;

    let mut problems = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), async {
        sessions_held.wait(&ctx).await?;
        status_held.wait(&ctx).await?;
        Ok::<_, CliError>(())
    })
    .await??;

    let live_sessions: Value = sessions_held
        .member("sessions")?
        .value()?
        .to_json()
        .to_serde()?;
    let mut live_by_id = BTreeMap::new();
    if let Some(arr) = live_sessions.as_array() {
        for item in arr {
            if let Some(id) = item["id"].as_str() {
                live_by_id.insert(id.to_owned(), item.clone());
            }
        }
    }
    let before_statuses: Value = status_held
        .member("statuses")?
        .value()?
        .to_json()
        .to_serde()?;

    for (id, expected) in &report.sessions {
        match live_by_id.get(id) {
            None => problems.push(format!("{id}: missing from p2.sessions")),
            Some(found) => {
                let actual_title = found["title"].as_str().unwrap_or_default();
                if actual_title != expected.title {
                    problems.push(format!(
                        "{id}: title {:?} -> {:?}",
                        expected.title, actual_title
                    ));
                }
            }
        }
    }

    for id in chosen_sessions(report) {
        let Some(expected) = report.sessions.get(&id) else {
            continue;
        };
        let was_running = before_statuses[&id]["running"].as_bool() == Some(true);
        for (conv, conv_report) in &expected.conversations {
            let Ok(conv_num) = conv.parse::<u64>() else {
                continue;
            };
            let chat = peer.hold(
                chat_contract.clone(),
                json!({ "session": id, "conversation": conv_num }).into(),
                &ctx,
            )?;
            let check = tokio::time::timeout(Duration::from_secs(60), async {
                chat.wait(&ctx).await?;
                let view_member = chat.member("view")?;
                let view: Value = loop {
                    let val: Value = view_member.value()?.to_json().to_serde()?;
                    if !val.is_null() {
                        break val;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                };
                let newest = view["entries"]
                    .as_array()
                    .and_then(|entries| entries.last())
                    .and_then(|entry| entry["id"].as_i64());
                if newest != Some(conv_report.newest) {
                    problems.push(format!(
                        "{id}/{conv}: newest entry {} -> {}",
                        conv_report.newest,
                        newest
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| "undefined".into())
                    ));
                }
                Ok::<_, CliError>(())
            })
            .await;
            chat.release();
            match check {
                Ok(Ok(())) => {}
                Ok(Err(err)) => problems.push(format!("{id}/{conv}: {err}")),
                Err(_) => {
                    problems.push(format!("{id}/{conv}: the view did not arrive within 60 s"))
                }
            }
        }
        if !was_running {
            let control = peer.hold(
                control_contract.clone(),
                json!({ "session": id }).into(),
                &ctx,
            )?;
            if control.wait(&ctx).await.is_ok() {
                let _ = control.call("restart", Value::Null.into(), &ctx).await;
            }
            control.release();
        }
    }

    sessions_held.release();
    status_held.release();
    Ok(problems)
}

pub async fn verify_sessions_command(args: &[String]) -> Result<(bool, String), CliError> {
    let data_dir = match option_value(args, "--data")? {
        Some(dir) => PathBuf::from(dir),
        None => data_directory()?,
    };
    let report = match option_value(args, "--report")? {
        Some(path) => serde_json::from_slice::<DataReport>(&fs::read(path)?)?,
        None => data_report(&data_dir).await?,
    };
    let port = port_from_env()?;
    let default_url = format!("http://127.0.0.1:{port}");
    let url = positional_arg(args).unwrap_or(&default_url);
    let problems = verify_sessions(url, &report, &data_dir).await?;
    let ok = problems.is_empty();
    let mut lines = problems;
    lines.push(if ok {
        "no differences".into()
    } else {
        format!("{} differences", lines.len())
    });
    Ok((ok, lines.join("\n")))
}
