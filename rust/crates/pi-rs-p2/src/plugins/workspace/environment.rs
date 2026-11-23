use std::{collections::VecDeque, sync::Arc};

use base64::{Engine, engine::general_purpose::STANDARD};
use pi_rs_chord::{Error, Result, context::Context};
use pi_rs_durable::env::{
    BinaryReader, ExecutionEnv, FileErrorCode, FileKind, OpenBinaryOptions, RemoveOptions,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(super) const CHUNK: u64 = 1024 * 1024;
pub(super) const LIMIT: u64 = 25 * CHUNK;

pub(super) fn invalid(message: impl ToString) -> Error {
    Error::Invalid(message.to_string())
}

pub(super) fn valid_path(path: &str) -> bool {
    path.is_empty()
        || (path.encode_utf16().count() <= 4096
            && !path.starts_with('~')
            && !path.contains(['\0', '\\'])
            && path.split('/').next() != Some(".git")
            && path.split('/').all(|part| !matches!(part, "" | "." | "..")))
}

pub(super) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.encode_utf16().count() <= 255
        && valid_path(name)
        && !name.contains('/')
        && !name.starts_with(".pi-upload-")
}

#[derive(Deserialize)]
pub(super) struct Read {
    pub path: String,
    pub start: Option<u64>,
    pub end: Option<u64>,
    pub suffix: Option<u64>,
}

#[derive(Deserialize, Serialize)]
pub(super) struct Bytes {
    pub size: u64,
    pub start: u64,
    pub data: String,
}

#[derive(Deserialize)]
pub(super) struct Upload {
    pub path: String,
    pub name: String,
    pub token: String,
    pub offset: u64,
    pub data: String,
    #[serde(rename = "final")]
    pub final_chunk: bool,
    #[serde(default)]
    pub abort: bool,
}

pub struct Workspace {
    pub(super) env: Arc<dyn ExecutionEnv>,
    pub(super) root: String,
}

impl Workspace {
    pub async fn open(env: Arc<dyn ExecutionEnv>, ctx: &Context) -> Result<Self> {
        let mut root = env.canonical_path(env.cwd(), ctx).await.map_err(invalid)?;
        if root != "/" {
            root.truncate(root.trim_end_matches('/').len());
        }
        Ok(Self { env, root })
    }

    pub(super) async fn resolve(&self, path: &str, ctx: &Context) -> Result<String> {
        if !valid_path(path) {
            return Err(invalid("Invalid workspace path"));
        }
        let absolute = join(&self.root, path);
        let canonical = self
            .env
            .canonical_path(&absolute, ctx)
            .await
            .map_err(|error| {
                invalid(if error.code == FileErrorCode::NotFound {
                    "File not found"
                } else {
                    "Cannot open path"
                })
            })?;
        let prefix = format!("{}/", self.root.trim_end_matches('/'));
        if canonical != self.root && !canonical.strip_prefix(&prefix).is_some_and(valid_path) {
            return Err(invalid("Path leaves the working directory"));
        }
        Ok(canonical)
    }

    async fn reader(
        &self,
        path: &str,
        viewing: bool,
        ctx: &Context,
    ) -> Result<Box<dyn BinaryReader>> {
        self.resolve(path, ctx).await?;
        self.env
            .open_binary_reader(
                &join(&self.root, path),
                Some(OpenBinaryOptions { no_follow: true }),
                ctx,
            )
            .await
            .map_err(|error| {
                invalid(if viewing && error.code == FileErrorCode::IsDirectory {
                    "Is a directory"
                } else {
                    "Not a regular file"
                })
            })
    }

    pub(super) async fn read(&self, range: Read, ctx: &Context) -> Result<Bytes> {
        let mut reader = self.reader(&range.path, false, ctx).await?;
        let result = async {
            let size = reader.info(ctx).await.map_err(invalid)?.size;
            let start = range.suffix.map_or(range.start.unwrap_or(0), |suffix| {
                size.saturating_sub(suffix)
            });
            let stop = range
                .end
                .map_or(size, |end| size.min(end.saturating_add(1)));
            let length = stop.saturating_sub(start).min(CHUNK);
            let data = if length == 0 {
                String::new()
            } else {
                STANDARD.encode(reader.read(start, length, ctx).await.map_err(invalid)?)
            };
            Ok(Bytes { size, start, data })
        }
        .await;
        reader.close(&Context::background()).await;
        result
    }

    pub(super) async fn preview(
        &self,
        path: &str,
        limit: u64,
        ctx: &Context,
    ) -> Result<(u64, Option<String>)> {
        let mut reader = self.reader(path, true, ctx).await?;
        let result = async {
            let size = reader.info(ctx).await.map_err(invalid)?.size;
            let bytes = reader
                .read(0, size.min(limit), ctx)
                .await
                .map_err(invalid)?;
            Ok((size, decode(&bytes, size > limit)))
        }
        .await;
        reader.close(&Context::background()).await;
        result
    }

    pub(super) async fn list(&self, path: &str, ctx: &Context) -> Result<Value> {
        let directory = self.resolve(path, ctx).await?;
        let mut reader = self
            .env
            .open_dir_reader(&directory, ctx)
            .await
            .map_err(|error| {
                invalid(if error.code == FileErrorCode::NotDirectory {
                    "Not a directory"
                } else {
                    "Cannot list directory"
                })
            })?;
        let result = async {
            let mut entries = Vec::new();
            loop {
                let page = reader.next(500, ctx).await.map_err(invalid)?;
                entries.extend(page.entries.into_iter().filter(|entry| !path.is_empty() || entry.name != ".git"));
                if entries.len() > 2000 || page.done {
                    break;
                }
            }
            let truncated = entries.len() > 2000;
            entries.truncate(2000);
            entries.sort_by_cached_key(|entry| (entry.kind != FileKind::Directory, entry.name.to_lowercase(), entry.name.clone()));
            let mut rows = Vec::with_capacity(entries.len());
            for batch in entries.chunks(64) {
                let args = ["check-ignore", "--"].into_iter().chain(batch.iter().map(|entry| entry.name.as_str())).collect::<Vec<_>>();
                let ignored = self.git_at(&args, CHUNK as usize, &directory, ctx).await.ok();
                for entry in batch {
                    let mut row = json!({"name":entry.name,"kind":entry.kind,"size":entry.size,"mtimeMs":entry.mtime_ms});
                    if ignored.as_ref().is_some_and(|run| run.code <= 1 && run.stdout.split('\n').any(|name| name == entry.name)) {
                        row["ignored"] = json!(true);
                    }
                    rows.push(row);
                }
            }
            Ok(json!({"kind":"list","path":path,"entries":rows,"truncated":truncated}))
        }.await;
        reader.close(&Context::background()).await;
        result
    }

    pub(super) async fn walk(&self, ctx: &Context) -> Result<Value> {
        let mut paths = Vec::new();
        let mut directories = VecDeque::from([String::new()]);
        for _ in 0..5000 {
            let Some(relative) = directories.pop_front() else {
                break;
            };
            let Ok(mut reader) = self
                .env
                .open_dir_reader(&join(&self.root, &relative), ctx)
                .await
            else {
                ctx.check().map_err(invalid)?;
                continue;
            };
            let mut entries = Vec::new();
            while let Ok(page) = reader.next(500, ctx).await {
                entries.extend(page.entries);
                if entries.len() > 100_000 || page.done {
                    break;
                }
            }
            reader.close(&Context::background()).await;
            ctx.check().map_err(invalid)?;
            let truncated = entries.len() > 100_000;
            entries.truncate(100_000);
            entries.sort_by(|left, right| left.name.encode_utf16().cmp(right.name.encode_utf16()));
            for entry in entries {
                if entry.name == ".git" {
                    continue;
                }
                let path = join_relative(&relative, &entry.name);
                if entry.kind == FileKind::Directory {
                    if entry.name != "node_modules" {
                        directories.push_back(path);
                    }
                } else {
                    if paths.len() == 50_000 {
                        return Ok(json!({"kind":"files","paths":paths,"truncated":true}));
                    }
                    paths.push(path);
                }
            }
            if truncated {
                return Ok(json!({"kind":"files","paths":paths,"truncated":true}));
            }
        }
        Ok(json!({"kind":"files","paths":paths,"truncated":!directories.is_empty()}))
    }

    pub(super) async fn write(&self, upload: Upload, ctx: &Context) -> Result<()> {
        if upload.token.len() != 36
            || !upload
                .token
                .bytes()
                .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f' | b'-'))
        {
            return Err(invalid("Invalid upload"));
        }
        if !valid_name(&upload.name) {
            return Err(invalid("Invalid file name"));
        }
        let dir = self.resolve(&upload.path, ctx).await?;
        let target = join(&dir, &upload.name);
        let temp = join(&dir, &format!(".pi-upload-{}", upload.token));
        let cleanup = || async {
            let _ = self
                .env
                .remove(
                    &temp,
                    Some(RemoveOptions {
                        force: true,
                        recursive: false,
                    }),
                    &Context::background(),
                )
                .await;
        };
        if upload.abort {
            cleanup().await;
            return Ok(());
        }
        let free = || async {
            if self.env.exists(&target, ctx).await.map_err(invalid)? {
                return Err(invalid(format!("{} already exists", upload.name)));
            }
            Ok(())
        };
        let bytes = STANDARD.decode(&upload.data).map_err(invalid)?;
        if bytes.len() as u64 > CHUNK || upload.offset.saturating_add(bytes.len() as u64) > LIMIT {
            return Err(invalid("Upload exceeds 25 MiB"));
        }
        let staged = match self.env.file_info(&temp, ctx).await {
            Ok(info) => Some(info),
            Err(error) if error.code == FileErrorCode::NotFound => None,
            Err(error) => return Err(invalid(error)),
        };
        if staged
            .as_ref()
            .is_some_and(|info| info.kind != FileKind::File || info.size != upload.offset)
            || (staged.is_none() && upload.offset != 0)
        {
            return Err(invalid("Invalid upload offset"));
        }
        if upload.offset == 0 {
            free().await?;
            self.env
                .write_file(&temp, &bytes, ctx)
                .await
                .map_err(invalid)?;
        } else {
            self.env
                .append_file(&temp, &bytes, ctx)
                .await
                .map_err(invalid)?;
        }
        if upload.final_chunk {
            let result = async {
                free().await?;
                self.env
                    .rename_file(&temp, &target, ctx)
                    .await
                    .map_err(invalid)
            }
            .await;
            if result.is_err() {
                cleanup().await;
            }
            result?;
        }
        Ok(())
    }
}

pub(super) fn join(base: &str, path: &str) -> String {
    if path.is_empty() {
        base.to_owned()
    } else {
        format!("{}/{path}", base.trim_end_matches('/'))
    }
}

fn join_relative(base: &str, path: &str) -> String {
    if base.is_empty() {
        path.to_owned()
    } else {
        join(base, path)
    }
}

fn decode(bytes: &[u8], truncated: bool) -> Option<String> {
    if bytes.iter().take(8192).any(|byte| *byte == 0) {
        return None;
    }
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) if truncated && error.error_len().is_none() => {
            std::str::from_utf8(&bytes[..error.valid_up_to()]).ok()?
        }
        Err(_) => return None,
    };
    Some(text.strip_prefix('\u{feff}').unwrap_or(text).to_owned())
}
