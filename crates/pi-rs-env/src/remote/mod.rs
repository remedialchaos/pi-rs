//! Durable filesystem and shell adapters over a shared pi-env connection.
use crate::{Connection, RemoteError, RemoteInfo, RemoteWatchOptions, Reply, RequestOptions};
use async_trait::async_trait;
use pi_rs_chord::context::Context;
use pi_rs_durable::env::*;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
mod local;
mod paths;
mod readers;
mod shell;
pub use local::LocalExecutionEnv;
use readers::{Directory, Handle, Lines, file_info};

const WRITE_DEPTH: usize = 8;
const MAX_SAFE: u64 = (1 << 53) - 1;
pub struct RemoteExecutionEnvOptions {
    pub id: String,
    pub cwd: String,
    pub connection: Arc<Connection>,
    pub shell_path: Option<String>,
    pub shell_env: BTreeMap<String, String>,
    pub watch: RemoteWatchOptions,
}
pub struct RemoteExecutionEnv {
    options: RemoteExecutionEnvOptions,
    running: Arc<Mutex<BTreeMap<u32, u64>>>,
}
fn failure(code: FileErrorCode, message: impl Into<String>, path: &str) -> FileError {
    FileError::new(code, message, Some(path.into()))
}
fn translate(error: RemoteError, path: &str) -> FileError {
    let e = crate::to_file_error(&error, Some(path));
    FileError::new(e.code.into(), e.message, e.path)
}
impl From<crate::FileErrorCode> for FileErrorCode {
    fn from(v: crate::FileErrorCode) -> Self {
        use crate::FileErrorCode as C;
        match v {
            C::Aborted => Self::Aborted,
            C::Invalid => Self::Invalid,
            C::NotFound => Self::NotFound,
            C::IsDirectory => Self::IsDirectory,
            C::NotDirectory => Self::NotDirectory,
            C::PermissionDenied => Self::PermissionDenied,
            C::Unknown => Self::Unknown,
        }
    }
}
fn check(ctx: &Context, path: &str) -> Result<(), FileError> {
    if ctx.is_aborted() {
        Err(failure(FileErrorCode::Aborted, "aborted", path))
    } else {
        Ok(())
    }
}
fn field<'a>(j: &'a Value, key: &str, path: &str) -> Result<&'a str, FileError> {
    j[key].as_str().ok_or_else(|| {
        failure(
            FileErrorCode::Unknown,
            format!("Invalid daemon field {key}"),
            path,
        )
    })
}
async fn transport<T: Send + 'static>(
    path: &str,
    operation: impl FnOnce() -> Result<T, RemoteError> + Send + 'static,
) -> Result<T, FileError> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|e| failure(FileErrorCode::Unknown, e.to_string(), path))?
        .map_err(|e| translate(e, path))
}
async fn request(
    c: &Arc<Connection>,
    op: &'static str,
    j: Value,
    options: RequestOptions,
    path: &str,
) -> Result<Reply, FileError> {
    let c = c.clone();
    transport(path, move || c.request(op, j, options)).await
}
// Drop may run on a Tokio executor or after its runtime has gone away.
fn detached_cleanup(cleanup: impl FnOnce() + Send + 'static) {
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn_blocking(cleanup);
    } else {
        let _ = std::thread::Builder::new()
            .name("pi-env-close".into())
            .spawn(cleanup);
    }
}
impl RemoteExecutionEnv {
    pub fn new(options: RemoteExecutionEnvOptions) -> Self {
        Self {
            options,
            running: Default::default(),
        }
    }
    pub fn set_cwd(&mut self, cwd: String) {
        self.options.cwd = cwd;
    }
    pub fn connection(&self) -> &Arc<Connection> {
        &self.options.connection
    }
    async fn op(
        &self,
        op: &'static str,
        path: &str,
        mut fields: Value,
    ) -> Result<Reply, FileError> {
        fields["path"] = json!(path);
        request(self.connection(), op, fields, Default::default(), path).await
    }
    async fn write(
        &self,
        path: &str,
        data: &[u8],
        append: bool,
        ctx: &Context,
    ) -> Result<(), FileError> {
        let p = self.absolute_path(path, ctx).await?;
        let mut chunks = data.chunks(512 * 1024);
        let first = chunks.next().unwrap_or_default();
        let keep = data.len() > first.len();
        let fields = json!({"path":p,"keep":keep,"append":append});
        let options = RequestOptions {
            payload: first.into(),
            ..Default::default()
        };
        if keep {
            let (mut h, _) =
                Handle::acquire(self.connection(), &p, "write", fields, options).await?;
            let mut pending = std::collections::VecDeque::new();
            // Submit each frame before starting the next one to preserve wire order.
            let mut tail = Ok(());
            let mut replies = Ok(());
            for chunk in chunks {
                if !append && let Err(error) = check(ctx, &p) {
                    tail = Err(error);
                    break;
                }
                match h.submit_chunk(chunk).await {
                    Ok(rx) => pending.push_back(rx),
                    Err(error) => {
                        tail = Err(error);
                        break;
                    }
                }
                if pending.len() == WRITE_DEPTH
                    && let Some(rx) = pending.pop_front()
                {
                    replies = chunk_reply(&p, rx).await;
                    if replies.is_err() {
                        break;
                    }
                }
            }
            for rx in pending {
                replies = replies.and(chunk_reply(&p, rx).await);
            }
            h.release().await;
            replies.and(tail)?;
        } else {
            request(self.connection(), "write", fields, options, &p).await?;
        }
        if append {
            check(ctx, &p)?;
        }
        Ok(())
    }
}
async fn chunk_reply(
    path: &str,
    rx: std::sync::mpsc::Receiver<Result<Reply, RemoteError>>,
) -> Result<(), FileError> {
    transport(path, move || {
        rx.recv()
            .map_err(|_| RemoteError::lost("pi-env connection lost"))?
            .map(|_| ())
    })
    .await
}
async fn remote_info(
    connection: &Arc<Connection>,
    path: &str,
    ctx: &Context,
) -> Result<RemoteInfo, FileError> {
    connection
        .info_with_context(ctx)
        .await
        .map_err(|e| translate(e, path))
}
pub async fn list_files_over(
    c: &Arc<Connection>,
    path: &str,
    opts: Option<ListFilesOptions>,
    ctx: &Context,
) -> Result<FileListing, FileError> {
    let opts = opts.unwrap_or_default();
    for (key, value) in [
        ("maxEntries", opts.max_entries),
        ("maxBytes", opts.max_bytes),
        ("timeoutMs", opts.timeout_ms),
    ] {
        if value.is_some_and(|n| n > MAX_SAFE) {
            return Err(failure(
                FileErrorCode::Invalid,
                format!("{key} must be a non-negative safe integer"),
                path,
            ));
        }
    }
    check(ctx, path)?;
    if !remote_info(c, path, ctx).await?.has_feature("listFiles") {
        return Err(failure(
            FileErrorCode::NotSupported,
            "The pi-env daemon does not list files; update it",
            path,
        ));
    }
    let mut j = json!(opts);
    j["path"] = json!(path);
    let controller = Arc::new(crate::AbortController::new());
    let cancel = controller.clone();
    let _subscription = ctx
        .abort_signal()
        .map(|s| s.subscribe(move |_| cancel.abort()));
    let r = request(
        c,
        "listFiles",
        j,
        RequestOptions {
            signal: Some(controller.signal()),
            ..Default::default()
        },
        path,
    )
    .await?;
    check(ctx, path)?;
    let entries = r
        .payload
        .split_inclusive(|b| *b == 0)
        .filter_map(|record| {
            let (kind, name) = record.strip_suffix(&[0])?.split_first()?;
            Some(FileListEntry {
                kind: match kind {
                    b'f' => FileKind::File,
                    b'd' => FileKind::Directory,
                    b'l' => FileKind::Symlink,
                    _ => return None,
                },
                path: String::from_utf8_lossy(name).into_owned(),
            })
        })
        .collect();
    Ok(FileListing {
        entries,
        truncated: r.json["truncated"] == true,
    })
}
#[async_trait]
impl FileSystem for RemoteExecutionEnv {
    fn id(&self) -> &str {
        &self.options.id
    }
    fn cwd(&self) -> &str {
        &self.options.cwd
    }
    async fn absolute_path(&self, path: &str, ctx: &Context) -> Result<String, FileError> {
        check(ctx, path)?;
        let info = remote_info(self.connection(), path, ctx).await?;
        check(ctx, path)?;
        Ok(paths::resolve(&info, self.cwd(), path))
    }
    async fn join_path(&self, parts: &[&str], ctx: &Context) -> Result<String, FileError> {
        Ok(paths::join(
            &remote_info(self.connection(), "", ctx).await?,
            parts,
        ))
    }
    async fn list_files(
        &self,
        path: &str,
        options: Option<ListFilesOptions>,
        ctx: &Context,
    ) -> Result<FileListing, FileError> {
        list_files_over(
            self.connection(),
            &self.absolute_path(path, ctx).await?,
            options,
            ctx,
        )
        .await
    }
    async fn file_info(&self, path: &str, ctx: &Context) -> Result<FileInfo, FileError> {
        let p = self.absolute_path(path, ctx).await?;
        file_info(
            &p,
            &self.op("lstat", &p, json!({})).await?.json,
            remote_info(self.connection(), &p, ctx)
                .await?
                .separator
                .as_str(),
        )
    }
    async fn open_binary_reader(
        &self,
        path: &str,
        opts: Option<OpenBinaryOptions>,
        ctx: &Context,
    ) -> Result<Box<dyn BinaryReader>, FileError> {
        Ok(Box::new(
            self.open(path, "open", json!(opts.unwrap_or_default()), ctx)
                .await?
                .0,
        ))
    }
    async fn open_text_line_reader(
        &self,
        path: &str,
        ctx: &Context,
    ) -> Result<Box<dyn TextLineReader>, FileError> {
        Ok(Box::new(Lines::new(
            self.open(path, "open", json!({"mode":"read"}), ctx)
                .await?
                .0,
        )))
    }
    async fn read_binary_file(&self, path: &str, ctx: &Context) -> Result<Vec<u8>, FileError> {
        let (h, j) = self.open(path, "open", json!({"mode":"read"}), ctx).await?;
        if let Some(e) = j.get("statError") {
            return Err(translate(RemoteError::new(e), &h.path));
        }
        let size = j["stat"]["size"]
            .as_u64()
            .filter(|_| j["stat"]["kind"] == "file")
            .unwrap_or(0);
        if size > i32::MAX as u64 {
            return Err(failure(
                FileErrorCode::Unknown,
                format!("File size ({size}) is greater than 2 GiB"),
                &h.path,
            ));
        }
        if size > 0 {
            return h.range(0, size, ctx).await;
        }
        let mut data = Vec::new();
        loop {
            check(ctx, &h.path)?;
            let bytes = h
                .send("pread", json!({"length":65536}), vec![])
                .await?
                .payload;
            if bytes.is_empty() {
                return Ok(data);
            }
            data.extend(bytes);
        }
    }
    async fn read_text_file(&self, path: &str, ctx: &Context) -> Result<String, FileError> {
        Ok(String::from_utf8_lossy(&self.read_binary_file(path, ctx).await?).into_owned())
    }
    async fn write_file(&self, path: &str, data: &[u8], ctx: &Context) -> Result<(), FileError> {
        self.write(path, data, false, ctx).await
    }
    async fn append_file(&self, path: &str, data: &[u8], ctx: &Context) -> Result<(), FileError> {
        self.write(path, data, true, ctx).await
    }
    async fn truncate_file(&self, path: &str, size: u64, ctx: &Context) -> Result<(), FileError> {
        let p = self.absolute_path(path, ctx).await?;
        if size > MAX_SAFE {
            return Err(failure(
                FileErrorCode::Invalid,
                "File size must be a non-negative safe integer",
                &p,
            ));
        }
        self.op("truncate", &p, json!({"size":size})).await?;
        check(ctx, &p)
    }
    async fn flush_file(&self, path: &str, ctx: &Context) -> Result<(), FileError> {
        let p = self.absolute_path(path, ctx).await?;
        self.op("fsync", &p, json!({})).await?;
        check(ctx, &p)
    }
    async fn rename_file(&self, source: &str, dest: &str, ctx: &Context) -> Result<(), FileError> {
        let p = self.absolute_path(source, ctx).await?;
        let d = self.absolute_path(dest, ctx).await?;
        self.op("rename", &p, json!({"to":d})).await?;
        Ok(())
    }
    async fn canonical_path(&self, path: &str, ctx: &Context) -> Result<String, FileError> {
        let p = self.absolute_path(path, ctx).await?;
        Ok(field(&self.op("realpath", &p, json!({})).await?.json, "path", &p)?.into())
    }
    async fn open_dir_reader(
        &self,
        path: &str,
        ctx: &Context,
    ) -> Result<Box<dyn DirReader>, FileError> {
        Ok(Box::new(Directory::new(
            self.open(path, "opendir", json!({}), ctx).await?.0,
        )))
    }
    async fn list_dir(&self, path: &str, ctx: &Context) -> Result<Vec<FileInfo>, FileError> {
        let mut dir = Directory::new(self.open(path, "opendir", json!({}), ctx).await?.0);
        let mut entries = Vec::new();
        while !dir.done {
            entries.extend(dir.page(1000, ctx).await?);
        }
        if dir.handle.separator != "/" {
            return Ok(entries.into_iter().map(|(_, info)| info).collect());
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(entries.into_iter().map(|(_, info)| info).collect())
    }
    async fn create_dir(
        &self,
        path: &str,
        opts: Option<CreateDirOptions>,
        ctx: &Context,
    ) -> Result<(), FileError> {
        self.op(
            "mkdir",
            &self.absolute_path(path, ctx).await?,
            json!(opts.unwrap_or_default()),
        )
        .await?;
        Ok(())
    }
    async fn remove(
        &self,
        path: &str,
        opts: Option<RemoveOptions>,
        ctx: &Context,
    ) -> Result<(), FileError> {
        self.op(
            "rm",
            &self.absolute_path(path, ctx).await?,
            json!(opts.unwrap_or_default()),
        )
        .await?;
        Ok(())
    }
    async fn create_temp_dir(
        &self,
        prefix: Option<&str>,
        ctx: &Context,
    ) -> Result<String, FileError> {
        check(ctx, "")?;
        let info = remote_info(self.connection(), "", ctx).await?;
        let p = paths::join(&info, &[&info.tmpdir, prefix.unwrap_or("tmp-")]);
        Ok(field(&self.op("mkdtemp", &p, json!({})).await?.json, "path", &p)?.into())
    }
    async fn create_temp_file(
        &self,
        opts: Option<TempFileOptions<'_>>,
        ctx: &Context,
    ) -> Result<String, FileError> {
        let dir = self.create_temp_dir(None, ctx).await?;
        let opts = opts.unwrap_or_default();
        let name = format!(
            "{}{}{}",
            opts.prefix.unwrap_or_default(),
            uuid::Uuid::new_v4(),
            opts.suffix.unwrap_or_default()
        );
        let p = self.join_path(&[&dir, &name], ctx).await?;
        self.op("write", &p, json!({"append":false,"parents":false}))
            .await?;
        Ok(p)
    }
    async fn watch(
        &self,
        targets: &[WatchTarget],
        on_change: WatchCallback,
        ctx: &Context,
    ) -> Result<Box<dyn FileWatcher>, FileError> {
        shell::watch(self, targets, on_change, ctx).await
    }
}
impl RemoteExecutionEnv {
    async fn open(
        &self,
        path: &str,
        op: &'static str,
        fields: Value,
        ctx: &Context,
    ) -> Result<(Handle, Value), FileError> {
        let p = self.absolute_path(path, ctx).await?;
        let mut fields = fields;
        fields["path"] = json!(p);
        let opened = Handle::acquire(self.connection(), &p, op, fields, Default::default()).await?;
        check(ctx, &p)?;
        Ok(opened)
    }
}
