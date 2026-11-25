//! Environment capability checks usable against a fresh local or remote fixture for each case.

use std::{error::Error, future::Future, path::Path, time::Duration};

use futures_util::future::BoxFuture;
use pi_rs_chord::context::Context;
use tokio::{
    sync::mpsc,
    time::{sleep, timeout},
};

use crate::env::{
    ExecutionEnv, ExecutionErrorCode, FileChange, FileError, FileErrorCode, FileKind,
    ListFilesOptions, OpenBinaryOptions, RemoveOptions, ScanLinesOptions, ShellCommand,
    ShellExecOptions, ShellOutputInfo, ShellOutputStream, ShellOutputWindow, WatchExclude,
    WatchTarget, range_decoder, starts_with_bom,
};

pub type EnvConformanceResult = Result<(), Box<dyn Error + Send + Sync>>;
type Test = for<'a> fn(
    &'a dyn ExecutionEnv,
    &'a EnvConformanceOptions,
) -> BoxFuture<'a, EnvConformanceResult>;

#[derive(Debug, Clone)]
pub struct EnvConformanceOptions {
    pub shell: Vec<String>,
    pub list_files: bool,
    pub symlinks: bool,
}

impl Default for EnvConformanceOptions {
    fn default() -> Self {
        Self {
            shell: vec!["sh".into(), "-c".into()],
            list_files: false,
            symlinks: true,
        }
    }
}

pub struct EnvConformanceCase {
    pub name: &'static str,
    pub timeout_ms: Option<u64>,
    test: Test,
    options: std::sync::Arc<EnvConformanceOptions>,
}

impl EnvConformanceCase {
    /// Run with a fresh, empty and writable current directory in `env`.
    pub async fn run(&self, env: &dyn ExecutionEnv) -> EnvConformanceResult {
        if let Some(ms) = self.timeout_ms {
            timeout(Duration::from_millis(ms), (self.test)(env, &self.options)).await?
        } else {
            (self.test)(env, &self.options).await
        }
    }
}

pub fn create_env_conformance(options: EnvConformanceOptions) -> Vec<EnvConformanceCase> {
    let options = std::sync::Arc::new(options);
    let mut cases = Vec::new();
    macro_rules! case {
        ($name:literal, $function:ident $(, $timeout:literal)?) => {
            cases.push(EnvConformanceCase {
                name: $name,
                timeout_ms: None$(.or(Some($timeout)))?,
                test: |env, options| Box::pin($function(env, options)),
                options: options.clone(),
            });
        };
    }
    case!(
        "binary reader reads byte ranges of the opened file",
        binary_ranges
    );
    case!(
        "binary reader scans lines like decoding the whole file",
        binary_scans
    );
    case!(
        "binary reader keeps reading the file it opened after a rename",
        binary_rename
    );
    case!(
        "binary reader refuses directories, missing files and aborted opens",
        binary_invalid
    );
    case!(
        "directory reader pages every entry exactly once",
        directory_pages
    );
    case!(
        "directory reader reports the end and refuses use after close",
        directory_end
    );
    case!(
        "directory reader refuses missing paths and files",
        directory_invalid
    );
    case!(
        "directory reader skips entries removed during enumeration",
        directory_removed
    );
    case!(
        "watch reports a missing file's creation, changes, replacement and removal",
        watch_file,
        30_000
    );
    case!(
        "watch reports a missing target whose ancestors are created",
        watch_ancestors,
        30_000
    );
    case!(
        "watch follows directories created together with their contents",
        watch_recursive,
        30_000
    );
    case!(
        "watch keeps watching a path whose parent is renamed and recreated",
        watch_recreated_parent,
        30_000
    );
    case!(
        "watch skips excluded entries and reports a rename out of them",
        watch_excludes,
        30_000
    );
    case!(
        "watch retains recursive coverage where a nonrecursive target overlaps",
        watch_overlaps,
        30_000
    );
    case!(
        "watch follows a directory replaced at the same path",
        watch_replaced,
        30_000
    );
    case!("watch stops reporting once stopped", watch_stop, 30_000);
    case!(
        "argv exec passes arguments without shell parsing",
        argv_literal
    );
    case!(
        "exec reports the stream of each chunk in both forms",
        exec_streams
    );
    case!("argv exec honors cwd and exit codes", argv_cwd);
    case!(
        "argv exec reports missing program and empty argv as spawn errors",
        argv_invalid
    );
    case!(
        "windowed exec keeps the exact tail and counts what it skips",
        exec_window
    );
    case!("argv exec distinguishes timeout and abort", exec_interrupt);
    if options.list_files {
        case!(
            "listFiles lists files and directories depth first in name order",
            list_order
        );
        case!(
            "listFiles skips what git ignores inside work trees, .git and excluded names",
            list_ignore
        );
        case!(
            "listFiles stops at its bounds and reports truncated",
            list_bounds
        );
        case!(
            "listFiles fails on missing directories, files, and aborts",
            list_invalid
        );
        if options.symlinks {
            case!(
                "listFiles lists symbolic links without following them",
                list_symlinks
            );
        }
    }
    if options.symlinks {
        case!(
            "binary reader follows symlinks unless noFollow refuses the final one",
            binary_symlinks
        );
        case!(
            "watch reports changes to the file a watched symbolic link points to",
            watch_symlink,
            30_000
        );
    }
    cases
}

macro_rules! verify {
    ($condition:expr) => {
        if !$condition {
            return Err(concat!("Conformance assertion failed: ", stringify!($condition)).into());
        }
    };
    ($condition:expr, $message:expr) => {
        if !$condition {
            return Err($message.into());
        }
    };
}

fn aborted() -> Context {
    let (context, cancel) = Context::background().with_cancel();
    cancel.cancel("conformance cancellation");
    context
}

fn code<T>(result: Result<T, FileError>) -> Option<FileErrorCode> {
    result.err().map(|error| error.code)
}

async fn binary_ranges(env: &dyn ExecutionEnv, _: &EnvConformanceOptions) -> EnvConformanceResult {
    let ctx = &Context::background();
    env.write_file("data.txt", b"hello world", ctx).await?;
    let mut reader = env.open_binary_reader("data.txt", None, ctx).await?;
    let info = reader.info(ctx).await?;
    verify!(info.name == "data.txt" && info.kind == FileKind::File && info.size == 11);
    verify!(reader.read(0, 5, ctx).await? == b"hello");
    verify!(reader.read(6, 100, ctx).await? == b"world");
    for (offset, length) in [(11, 4), (50, 1), (3, 0)] {
        verify!(reader.read(offset, length, ctx).await?.is_empty());
    }
    verify!(code(reader.read(u64::MAX, 1, ctx).await) == Some(FileErrorCode::Invalid));
    verify!(code(reader.read(0, u64::MAX, ctx).await) == Some(FileErrorCode::Invalid));
    verify!(code(reader.read(0, 1, &aborted()).await) == Some(FileErrorCode::Aborted));
    reader.close(ctx).await;
    reader.close(ctx).await;
    verify!(code(reader.read(0, 1, ctx).await) == Some(FileErrorCode::Invalid));
    verify!(code(reader.info(ctx).await) == Some(FileErrorCode::Invalid));
    Ok(())
}

fn decode(bytes: &[u8], at_start: bool) -> String {
    let mut decoder = range_decoder();
    let mut text = decoder.decode(Some(if at_start && starts_with_bom(bytes) {
        &bytes[3..]
    } else {
        bytes
    }));
    text.push_str(&decoder.decode(None));
    text
}

async fn binary_scans(env: &dyn ExecutionEnv, _: &EnvConformanceOptions) -> EnvConformanceResult {
    let ctx = &Context::background();
    let bytes = b"\xef\xbb\xbfa\n\xe2\x82\n\n\xef\xbb\xbfb\n\xc3\xa9";
    env.write_file("lines.txt", bytes, ctx).await?;
    let text = decode(bytes, true);
    let lines: Vec<_> = text.split('\n').collect();
    let mut reader = env.open_binary_reader("lines.txt", None, ctx).await?;
    for (start_line, end_line) in [
        (0, None),
        (0, Some(1)),
        (1, Some(3)),
        (2, Some(3)),
        (3, None),
        (4, Some(9)),
    ] {
        let scan = reader
            .scan_lines(
                ScanLinesOptions {
                    start_line,
                    end_line,
                },
                ctx,
            )
            .await?;
        let first = start_line as usize;
        let end = end_line.map_or(lines.len(), |end| (end as usize).min(lines.len()));
        let selected = lines[first..end].join("\n");
        let start = scan.start as usize;
        verify!(scan.newlines == (lines.len() - 1) as u64);
        verify!(
            scan.start <= scan.first_line_end
                && scan.first_line_end <= scan.end
                && scan.end <= bytes.len() as u64
        );
        verify!(decode(&bytes[start..scan.end as usize], start == 0) == selected);
        verify!(scan.selected_bytes == selected.len() as u64);
        verify!(decode(&bytes[start..scan.first_line_end as usize], start == 0) == lines[first]);
        verify!(scan.first_line_bytes == lines[first].len() as u64);
        verify!(scan.last_line_start <= scan.end && scan.last_line_start >= scan.start);
        verify!(
            decode(
                &bytes[scan.last_line_start as usize..scan.end as usize],
                scan.last_line_start == 0
            ) == lines[end - 1]
        );
    }
    let scan = reader
        .scan_lines(
            ScanLinesOptions {
                start_line: 9,
                end_line: None,
            },
            ctx,
        )
        .await?;
    verify!(scan.start == bytes.len() as u64 && scan.end == scan.start && scan.selected_bytes == 0);
    verify!(
        code(
            reader
                .scan_lines(
                    ScanLinesOptions {
                        start_line: 2,
                        end_line: Some(2)
                    },
                    ctx
                )
                .await
        ) == Some(FileErrorCode::Invalid)
    );
    reader.close(ctx).await;
    Ok(())
}

async fn binary_rename(env: &dyn ExecutionEnv, _: &EnvConformanceOptions) -> EnvConformanceResult {
    let ctx = &Context::background();
    env.write_file("a.txt", b"one", ctx).await?;
    let mut reader = env.open_binary_reader("a.txt", None, ctx).await?;
    env.rename_file("a.txt", "b.txt", ctx).await?;
    env.write_file("a.txt", b"replacement", ctx).await?;
    verify!(reader.read(0, 10, ctx).await? == b"one");
    verify!(reader.info(ctx).await?.size == 3);
    reader.close(ctx).await;
    Ok(())
}

async fn binary_invalid(env: &dyn ExecutionEnv, _: &EnvConformanceOptions) -> EnvConformanceResult {
    let ctx = &Context::background();
    env.create_dir("dir", None, ctx).await?;
    env.write_file("file.txt", b"x", ctx).await?;
    verify!(
        code(env.open_binary_reader("dir", None, ctx).await) == Some(FileErrorCode::IsDirectory)
    );
    verify!(
        code(env.open_binary_reader("missing", None, ctx).await) == Some(FileErrorCode::NotFound)
    );
    verify!(
        code(env.open_binary_reader("file.txt", None, &aborted()).await)
            == Some(FileErrorCode::Aborted)
    );
    Ok(())
}

async fn directory_pages(
    env: &dyn ExecutionEnv,
    _: &EnvConformanceOptions,
) -> EnvConformanceResult {
    let ctx = &Context::background();
    for path in ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"] {
        env.write_file(path, path.as_bytes(), ctx).await?;
    }
    env.create_dir("sub", None, ctx).await?;
    let mut reader = env.open_dir_reader(".", ctx).await?;
    let mut entries = Vec::new();
    for _ in 0..4 {
        let page = reader.next(2, ctx).await?;
        verify!(page.entries.len() <= 2);
        entries.extend(page.entries);
        if page.done {
            reader.close(ctx).await;
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            verify!(
                entries
                    .iter()
                    .map(|entry| entry.name.as_str())
                    .eq(["a.txt", "b.txt", "c.txt", "d.txt", "e.txt", "sub"])
            );
            verify!(
                entries[0].kind == FileKind::File
                    && entries[0].size == 5
                    && entries[5].kind == FileKind::Directory
            );
            return Ok(());
        }
    }
    Err("Directory did not reach its end".into())
}

async fn directory_end(env: &dyn ExecutionEnv, _: &EnvConformanceOptions) -> EnvConformanceResult {
    let ctx = &Context::background();
    env.create_dir("empty", None, ctx).await?;
    let mut reader = env.open_dir_reader("empty", ctx).await?;
    for _ in 0..2 {
        let page = reader.next(10, ctx).await?;
        verify!(page.done && page.entries.is_empty());
    }
    verify!(code(reader.next(0, ctx).await) == Some(FileErrorCode::Invalid));
    verify!(code(reader.next(1, &aborted()).await) == Some(FileErrorCode::Aborted));
    reader.close(ctx).await;
    reader.close(ctx).await;
    verify!(code(reader.next(1, ctx).await) == Some(FileErrorCode::Invalid));
    Ok(())
}

async fn directory_invalid(
    env: &dyn ExecutionEnv,
    _: &EnvConformanceOptions,
) -> EnvConformanceResult {
    let ctx = &Context::background();
    env.write_file("file.txt", b"x", ctx).await?;
    verify!(code(env.open_dir_reader("missing", ctx).await) == Some(FileErrorCode::NotFound));
    verify!(code(env.open_dir_reader("file.txt", ctx).await) == Some(FileErrorCode::NotDirectory));
    verify!(code(env.open_dir_reader(".", &aborted()).await) == Some(FileErrorCode::Aborted));
    Ok(())
}

async fn directory_removed(
    env: &dyn ExecutionEnv,
    _: &EnvConformanceOptions,
) -> EnvConformanceResult {
    let ctx = &Context::background();
    let paths = ["dir/x", "dir/y", "dir/z"];
    for path in paths {
        env.write_file(path, b"", ctx).await?;
    }
    let mut reader = env.open_dir_reader("dir", ctx).await?;
    for path in paths {
        env.remove(path, None, ctx).await?;
    }
    let page = reader.next(10, ctx).await?;
    verify!(page.entries.is_empty() && page.done);
    reader.close(ctx).await;
    Ok(())
}

fn target(path: &str, recursive: bool) -> WatchTarget {
    WatchTarget {
        path: path.into(),
        recursive,
        exclude: None,
    }
}

struct WatchSession<'a> {
    env: &'a dyn ExecutionEnv,
    ctx: &'a Context,
    recv: mpsc::UnboundedReceiver<FileChange>,
    seen: Vec<FileChange>,
}

impl WatchSession<'_> {
    async fn expect(
        &mut self,
        path: &str,
        change: impl Future<Output = Result<(), FileError>>,
    ) -> EnvConformanceResult {
        let path = self.env.absolute_path(path, self.ctx).await?;
        while let Ok(event) = self.recv.try_recv() {
            self.seen.push(event);
        }
        change.await?;
        timeout(Duration::from_secs(3), async {
            loop {
                let event = self.recv.recv().await.ok_or("Watcher stopped reporting")?;
                let covers = match &event {
                    FileChange::Overflow => true,
                    FileChange::Paths { paths } => paths
                        .iter()
                        .any(|parent| Path::new(&path).starts_with(parent)),
                    FileChange::Failed { error } => {
                        return Err(format!("Unexpected watch failure: {error}").into());
                    }
                };
                self.seen.push(event);
                if covers {
                    return Ok(());
                }
            }
        })
        .await
        .map_err(|_| format!("No change reported {path}; got {:?}", self.seen))?
    }
}

async fn watching(
    env: &dyn ExecutionEnv,
    ctx: &Context,
    targets: &[WatchTarget],
    run: impl AsyncFnOnce(&mut WatchSession<'_>) -> EnvConformanceResult,
) -> EnvConformanceResult {
    let (send, recv) = mpsc::unbounded_channel();
    let mut watcher = env
        .watch(
            targets,
            Box::new(move |event| {
                let _ = send.send(event);
            }),
            ctx,
        )
        .await?;
    let mut session = WatchSession {
        env,
        ctx,
        recv,
        seen: Vec::new(),
    };
    let result = run(&mut session).await;
    watcher.stop(ctx).await;
    result
}

async fn watch_file(env: &dyn ExecutionEnv, _: &EnvConformanceOptions) -> EnvConformanceResult {
    let ctx = &Context::background();
    watching(env, ctx, &[target("AGENTS.md", false)], async |session| {
        session
            .expect("AGENTS.md", env.write_file("AGENTS.md", b"one", ctx))
            .await?;
        session
            .expect("AGENTS.md", env.write_file("AGENTS.md", b"two!", ctx))
            .await?;
        session
            .expect("AGENTS.md", async {
                env.write_file("AGENTS.md.tmp", b"three", ctx).await?;
                env.rename_file("AGENTS.md.tmp", "AGENTS.md", ctx).await
            })
            .await?;
        session
            .expect("AGENTS.md", env.write_file("AGENTS.md", b"four", ctx))
            .await?;
        session
            .expect("AGENTS.md", env.remove("AGENTS.md", None, ctx))
            .await
    })
    .await
}

async fn watch_ancestors(
    env: &dyn ExecutionEnv,
    _: &EnvConformanceOptions,
) -> EnvConformanceResult {
    let ctx = &Context::background();
    let path = "a/b/c/AGENTS.md";
    watching(env, ctx, &[target(path, false)], async |session| {
        session.expect(path, env.write_file(path, b"x", ctx)).await
    })
    .await
}

async fn watch_recursive(
    env: &dyn ExecutionEnv,
    _: &EnvConformanceOptions,
) -> EnvConformanceResult {
    let ctx = &Context::background();
    env.create_dir("skills", None, ctx).await?;
    watching(env, ctx, &[target("skills", true)], async |session| {
        for (path, content) in [
            ("skills/a/b/SKILL.md", &b"one"[..]),
            ("skills/a/b/SKILL.md", &b"two!"[..]),
            ("skills/a/b/c/SKILL.md", &b"deeper"[..]),
        ] {
            session
                .expect(path, env.write_file(path, content, ctx))
                .await?;
        }
        Ok(())
    })
    .await
}

async fn watch_recreated_parent(
    env: &dyn ExecutionEnv,
    _: &EnvConformanceOptions,
) -> EnvConformanceResult {
    let ctx = &Context::background();
    env.write_file("proj/.pi/skills/x.md", b"x", ctx).await?;
    watching(
        env,
        ctx,
        &[target("proj/.pi/skills", true)],
        async |session| {
            session
                .expect(
                    "proj/.pi/skills",
                    env.rename_file("proj/.pi", "proj/old", ctx),
                )
                .await?;
            session
                .expect(
                    "proj/.pi/skills/y.md",
                    env.write_file("proj/.pi/skills/y.md", b"y", ctx),
                )
                .await?;
            session
                .expect(
                    "proj/.pi/skills/y.md",
                    env.write_file("proj/.pi/skills/y.md", b"yy", ctx),
                )
                .await
        },
    )
    .await
}

async fn watch_excludes(env: &dyn ExecutionEnv, _: &EnvConformanceOptions) -> EnvConformanceResult {
    let ctx = &Context::background();
    env.create_dir("skills", None, ctx).await?;
    let target = WatchTarget {
        exclude: Some(WatchExclude {
            hidden: true,
            names: vec!["node_modules".into()],
        }),
        ..target("skills", true)
    };
    watching(env, ctx, &[target], async |session| {
        env.write_file("skills/node_modules/dep/SKILL.md", b"dep", ctx)
            .await?;
        env.write_file("skills/.SKILL.md.tmp", b"draft", ctx)
            .await?;
        session
            .expect(
                "skills/SKILL.md",
                env.rename_file("skills/.SKILL.md.tmp", "skills/SKILL.md", ctx),
            )
            .await?;
        let hidden = [
            env.absolute_path("skills/node_modules", ctx).await?,
            env.absolute_path("skills/.SKILL.md.tmp", ctx).await?,
        ];
        for event in &session.seen {
            if let FileChange::Paths { paths } = event {
                verify!(
                    !paths.iter().any(|path| hidden
                        .iter()
                        .any(|excluded| Path::new(path).starts_with(excluded))),
                    format!("Excluded paths reported: {paths:?}")
                );
            }
        }
        Ok(())
    })
    .await
}

async fn watch_overlaps(env: &dyn ExecutionEnv, _: &EnvConformanceOptions) -> EnvConformanceResult {
    let ctx = &Context::background();
    env.write_file("skills/a/one.md", b"one", ctx).await?;
    watching(
        env,
        ctx,
        &[target("skills", false), target("skills", true)],
        async |session| {
            session
                .expect(
                    "skills/a/two.md",
                    env.write_file("skills/a/two.md", b"two", ctx),
                )
                .await
        },
    )
    .await
}

async fn watch_replaced(env: &dyn ExecutionEnv, _: &EnvConformanceOptions) -> EnvConformanceResult {
    let ctx = &Context::background();
    env.write_file("skills/a/x.md", b"x", ctx).await?;
    watching(env, ctx, &[target("skills", true)], async |session| {
        session
            .expect("skills/a", async {
                env.rename_file("skills/a", "skills-old", ctx).await?;
                env.create_dir("skills/a", None, ctx).await
            })
            .await?;
        session
            .expect("skills/a/y.md", env.write_file("skills/a/y.md", b"y", ctx))
            .await?;
        session
            .expect("skills/a/y.md", env.write_file("skills/a/y.md", b"yy", ctx))
            .await
    })
    .await
}

async fn watch_stop(env: &dyn ExecutionEnv, _: &EnvConformanceOptions) -> EnvConformanceResult {
    let ctx = &Context::background();
    let (send, mut recv) = mpsc::unbounded_channel();
    let mut watcher = env
        .watch(
            &[target("file.txt", false)],
            Box::new(move |event| {
                let _ = send.send(event);
            }),
            ctx,
        )
        .await?;
    watcher.stop(ctx).await;
    watcher.stop(ctx).await;
    env.write_file("file.txt", b"x", ctx).await?;
    sleep(Duration::from_millis(300)).await;
    verify!(recv.try_recv().is_err());
    Ok(())
}

fn argv<'a>(options: &'a EnvConformanceOptions, args: &[&'a str]) -> Vec<&'a str> {
    options
        .shell
        .iter()
        .map(String::as_str)
        .chain(args.iter().copied())
        .collect()
}

async fn collect(
    env: &dyn ExecutionEnv,
    command: ShellCommand<'_>,
    cwd: Option<&str>,
    ctx: &Context,
) -> Result<(i32, String, String), crate::env::ExecutionError> {
    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut output = |text: &str, _: &Context, info: ShellOutputInfo| {
        match info.stream {
            ShellOutputStream::Stdout => &mut stdout,
            ShellOutputStream::Stderr => &mut stderr,
        }
        .push_str(text);
        Ok(())
    };
    let result = env
        .exec(
            command,
            Some(ShellExecOptions {
                cwd,
                on_output: Some(&mut output),
                ..Default::default()
            }),
            ctx,
        )
        .await?;
    Ok((result.exit_code, stdout, stderr))
}

async fn argv_literal(
    env: &dyn ExecutionEnv,
    options: &EnvConformanceOptions,
) -> EnvConformanceResult {
    let ctx = &Context::background();
    let literal = "it's $(printf x > marker) `printf x > marker` *; printf x > marker";
    let args = argv(
        options,
        &[r#"printf "%s|%s" "$1" "$2""#, "argv0", literal, "a b"],
    );
    let (exit, stdout, _) = collect(env, ShellCommand::Argv(&args), None, ctx).await?;
    verify!(exit == 0 && stdout == format!("{literal}|a b"));
    verify!(!env.exists("marker", ctx).await?);
    Ok(())
}

async fn exec_streams(
    env: &dyn ExecutionEnv,
    options: &EnvConformanceOptions,
) -> EnvConformanceResult {
    let ctx = &Context::background();
    let script = "printf out; printf err >&2; printf more";
    let args = argv(options, &[script]);
    for command in [ShellCommand::Argv(&args), ShellCommand::Script(script)] {
        let (exit, stdout, stderr) = collect(env, command, None, ctx).await?;
        verify!(exit == 0 && stdout == "outmore" && stderr == "err");
    }
    Ok(())
}

async fn argv_cwd(env: &dyn ExecutionEnv, options: &EnvConformanceOptions) -> EnvConformanceResult {
    let ctx = &Context::background();
    env.create_dir("sub", None, ctx).await?;
    let args = argv(options, &["printf x > made.txt; exit 3"]);
    let (exit, _, _) = collect(env, ShellCommand::Argv(&args), Some("sub"), ctx).await?;
    verify!(exit == 3 && env.read_text_file("sub/made.txt", ctx).await? == "x");
    Ok(())
}

async fn argv_invalid(env: &dyn ExecutionEnv, _: &EnvConformanceOptions) -> EnvConformanceResult {
    let ctx = &Context::background();
    for args in [&["pi-durable-conformance-missing-program"][..], &[]] {
        let result = env.exec(ShellCommand::Argv(args), None, ctx).await;
        verify!(result.err().map(|error| error.code) == Some(ExecutionErrorCode::SpawnError));
    }
    Ok(())
}

async fn exec_window(
    env: &dyn ExecutionEnv,
    options: &EnvConformanceOptions,
) -> EnvConformanceResult {
    let ctx = &Context::background();
    let args = argv(
        options,
        &["i=0; while [ $i -lt 2000 ]; do echo line-$i; i=$((i+1)); done"],
    );
    let window = ShellOutputWindow {
        max_bytes: 200,
        max_lines: 5,
        min_interval_ms: 0,
        bytes_per_second: 1_000_000_000,
    };
    let mut bytes = 0;
    let mut newlines = 0;
    let mut tail = String::new();
    let mut output = |text: &str, _: &Context, info: ShellOutputInfo| {
        let after_newlines = text.bytes().filter(|byte| *byte == b'\n').count();
        if let Some(skip) = info.skipped {
            if text.len() <= window.max_bytes && after_newlines <= window.max_lines {
                return Err(crate::env::ExecutionError::new(
                    ExecutionErrorCode::CallbackError,
                    "A skip must be followed by more than the window",
                ));
            }
            bytes += skip.bytes;
            newlines += skip.newlines;
            tail.clear();
        }
        bytes += text.len();
        newlines += after_newlines;
        tail.push_str(text);
        Ok(())
    };
    let result = env
        .exec(
            ShellCommand::Argv(&args),
            Some(ShellExecOptions {
                window: Some(window),
                on_output: Some(&mut output),
                ..Default::default()
            }),
            ctx,
        )
        .await?;
    let expected: String = (0..2000).map(|index| format!("line-{index}\n")).collect();
    verify!(result.exit_code == 0 && bytes == expected.len() && newlines == 2000);
    verify!(tail.ends_with("line-1995\nline-1996\nline-1997\nline-1998\nline-1999\n"));
    Ok(())
}

async fn exec_interrupt(
    env: &dyn ExecutionEnv,
    options: &EnvConformanceOptions,
) -> EnvConformanceResult {
    let ctx = &Context::background();
    let args = argv(options, &["sleep 2"]);
    let timed_out = env
        .exec(
            ShellCommand::Argv(&args),
            Some(ShellExecOptions {
                timeout: Some(0.1),
                ..Default::default()
            }),
            ctx,
        )
        .await;
    verify!(timed_out.err().map(|error| error.code) == Some(ExecutionErrorCode::Timeout));
    let (ctx, cancel) = ctx.with_cancel();
    let (result, ()) = tokio::join!(env.exec(ShellCommand::Argv(&args), None, &ctx), async {
        sleep(Duration::from_millis(100)).await;
        cancel.cancel("conformance request");
    });
    verify!(result.err().map(|error| error.code) == Some(ExecutionErrorCode::Aborted));
    Ok(())
}

async fn fixtures(env: &dyn ExecutionEnv, paths: &[&str], ctx: &Context) -> EnvConformanceResult {
    for path in paths {
        env.write_file(path, b"", ctx).await?;
    }
    Ok(())
}

async fn listed(
    env: &dyn ExecutionEnv,
    path: &str,
    options: Option<ListFilesOptions>,
    ctx: &Context,
) -> Result<(Vec<String>, bool), FileError> {
    let listing = env.list_files(path, options, ctx).await?;
    let entries = listing
        .entries
        .into_iter()
        .map(|entry| {
            let kind = match entry.kind {
                FileKind::File => 'f',
                FileKind::Directory => 'd',
                FileKind::Symlink => 'l',
            };
            format!("{kind} {}", entry.path)
        })
        .collect();
    Ok((entries, listing.truncated))
}

async fn list_order(env: &dyn ExecutionEnv, _: &EnvConformanceOptions) -> EnvConformanceResult {
    let ctx = &Context::background();
    fixtures(
        env,
        &["b.txt", "a/z.txt", "a/b/c.txt", ".hidden/x", "A.txt"],
        ctx,
    )
    .await?;
    env.create_dir("empty", None, ctx).await?;
    let (all, truncated) = listed(env, ".", None, ctx).await?;
    verify!(
        !truncated
            && all
                == [
                    "d .hidden",
                    "f .hidden/x",
                    "f A.txt",
                    "d a",
                    "d a/b",
                    "f a/b/c.txt",
                    "f a/z.txt",
                    "f b.txt",
                    "d empty"
                ]
    );
    let (visible, _) = listed(
        env,
        ".",
        Some(ListFilesOptions {
            hidden: Some(false),
            ..Default::default()
        }),
        ctx,
    )
    .await?;
    verify!(
        visible
            == [
                "f A.txt",
                "d a",
                "d a/b",
                "f a/b/c.txt",
                "f a/z.txt",
                "f b.txt",
                "d empty"
            ]
    );
    for path in ["a", &env.absolute_path("a", ctx).await?] {
        let (entries, truncated) = listed(env, path, None, ctx).await?;
        verify!(!truncated && entries == ["d b", "f b/c.txt", "f z.txt"]);
    }
    verify!(listed(env, "empty", None, ctx).await? == (Vec::new(), false));
    Ok(())
}

async fn list_ignore(env: &dyn ExecutionEnv, _: &EnvConformanceOptions) -> EnvConformanceResult {
    let ctx = &Context::background();
    fixtures(
        env,
        &[
            "keep.txt",
            "out/x.js",
            "sub/keep.txt",
            "sub/drop.log",
            "node_modules/m/i.js",
        ],
        ctx,
    )
    .await?;
    env.write_file(".gitignore", b"out/\n*.log\n", ctx).await?;
    env.write_file("sub/.gitignore", b"!keep.txt\n", ctx)
        .await?;
    verify!(
        listed(env, ".", None, ctx)
            .await?
            .0
            .iter()
            .any(|path| path == "f out/x.js")
    );
    env.write_file(".git/HEAD", b"ref: refs/heads/main\n", ctx)
        .await?;
    env.write_file(".git/info/exclude", b"keep.txt\n", ctx)
        .await?;
    let (entries, _) = listed(
        env,
        ".",
        Some(ListFilesOptions {
            exclude: vec!["node_modules".into()],
            ..Default::default()
        }),
        ctx,
    )
    .await?;
    verify!(
        entries
            == [
                "f .gitignore",
                "d sub",
                "f sub/.gitignore",
                "f sub/keep.txt"
            ]
    );
    verify!(listed(env, "sub", None, ctx).await?.0 == ["f .gitignore", "f keep.txt"]);
    let all = listed(
        env,
        ".",
        Some(ListFilesOptions {
            gitignore: Some(false),
            ..Default::default()
        }),
        ctx,
    )
    .await?
    .0;
    verify!(
        ["f out/x.js", "f node_modules/m/i.js", "f keep.txt"]
            .iter()
            .all(|path| all.iter().any(|entry| entry == path))
    );
    verify!(
        !all.iter()
            .any(|entry| entry.contains(".git/") || entry == "d .git")
    );
    Ok(())
}

async fn list_bounds(env: &dyn ExecutionEnv, _: &EnvConformanceOptions) -> EnvConformanceResult {
    let ctx = &Context::background();
    fixtures(env, &["a", "b", "c"], ctx).await?;
    for (opts, expected, was_truncated) in [
        (
            ListFilesOptions {
                max_entries: Some(2),
                ..Default::default()
            },
            "ab",
            true,
        ),
        (
            ListFilesOptions {
                max_entries: Some(3),
                ..Default::default()
            },
            "abc",
            false,
        ),
        (
            ListFilesOptions {
                max_bytes: Some(6),
                ..Default::default()
            },
            "ab",
            true,
        ),
        (
            ListFilesOptions {
                max_entries: Some(0),
                ..Default::default()
            },
            "",
            true,
        ),
    ] {
        let (entries, truncated) = listed(env, ".", Some(opts), ctx).await?;
        verify!(
            truncated == was_truncated
                && entries
                    .iter()
                    .map(|entry| &entry[2..])
                    .eq(expected.split("").filter(|part| !part.is_empty()))
        );
    }
    let invalid = ListFilesOptions {
        max_entries: Some(u64::MAX),
        ..Default::default()
    };
    verify!(code(env.list_files(".", Some(invalid), ctx).await) == Some(FileErrorCode::Invalid));
    Ok(())
}

async fn list_invalid(env: &dyn ExecutionEnv, _: &EnvConformanceOptions) -> EnvConformanceResult {
    let ctx = &Context::background();
    env.write_file("file.txt", b"", ctx).await?;
    verify!(code(env.list_files("missing", None, ctx).await) == Some(FileErrorCode::NotFound));
    verify!(code(env.list_files("file.txt", None, ctx).await) == Some(FileErrorCode::NotDirectory));
    verify!(code(env.list_files(".", None, &aborted()).await) == Some(FileErrorCode::Aborted));
    Ok(())
}

async fn link(
    env: &dyn ExecutionEnv,
    options: &EnvConformanceOptions,
    script: &str,
    ctx: &Context,
) -> EnvConformanceResult {
    let cmd = argv(options, &[script]);
    verify!(
        env.exec(ShellCommand::Argv(&cmd), None, ctx)
            .await?
            .exit_code
            == 0
    );
    Ok(())
}

async fn list_symlinks(
    env: &dyn ExecutionEnv,
    options: &EnvConformanceOptions,
) -> EnvConformanceResult {
    let ctx = &Context::background();
    env.write_file("dir/inner.txt", b"", ctx).await?;
    link(
        env,
        options,
        "ln -s dir link && ln -s missing dangling && ln -s .. dir/loop",
        ctx,
    )
    .await?;
    verify!(
        listed(env, ".", None, ctx).await?.0
            == [
                "l dangling",
                "d dir",
                "f dir/inner.txt",
                "l dir/loop",
                "l link"
            ]
    );
    verify!(listed(env, "link", None, ctx).await?.0 == ["f inner.txt", "l loop"]);
    Ok(())
}

async fn binary_symlinks(
    env: &dyn ExecutionEnv,
    options: &EnvConformanceOptions,
) -> EnvConformanceResult {
    let ctx = &Context::background();
    env.write_file("target.txt", b"target", ctx).await?;
    env.write_file("sub/inner.txt", b"inner", ctx).await?;
    link(
        env,
        options,
        "ln -s target.txt link.txt && ln -s sub dirlink",
        ctx,
    )
    .await?;
    let mut followed = env.open_binary_reader("link.txt", None, ctx).await?;
    verify!(followed.read(0, 10, ctx).await? == b"target");
    followed.close(ctx).await;
    let no_follow = Some(OpenBinaryOptions { no_follow: true });
    verify!(
        code(env.open_binary_reader("link.txt", no_follow, ctx).await)
            == Some(FileErrorCode::Invalid)
    );
    let mut inner = env
        .open_binary_reader("dirlink/inner.txt", no_follow, ctx)
        .await?;
    verify!(inner.read(0, 10, ctx).await? == b"inner");
    inner.close(ctx).await;
    env.remove(
        "dirlink",
        Some(RemoveOptions {
            recursive: true,
            force: false,
        }),
        ctx,
    )
    .await?;
    verify!(env.exists("sub/inner.txt", ctx).await?);
    Ok(())
}

async fn watch_symlink(
    env: &dyn ExecutionEnv,
    options: &EnvConformanceOptions,
) -> EnvConformanceResult {
    let ctx = &Context::background();
    env.write_file("data/real.md", b"one", ctx).await?;
    env.create_dir("config", None, ctx).await?;
    link(env, options, "ln -s ../data/real.md config/AGENTS.md", ctx).await?;
    watching(
        env,
        ctx,
        &[target("config/AGENTS.md", false)],
        async |session| {
            session
                .expect(
                    "config/AGENTS.md",
                    env.write_file("data/real.md", b"two!", ctx),
                )
                .await
        },
    )
    .await
}
