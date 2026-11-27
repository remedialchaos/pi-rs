use std::{path::Path, time::Duration};

use pi_rs_chord::context::Context;
use pi_rs_durable::{
    env::{
        FileChange, FileErrorCode, FileSystem, LineScan, LineScanner, LocalEnvOptions,
        LocalExecutionEnv, RemoveOptions, StreamDecoder, TextLine, WatchMode, WatchOptions,
        WatchTarget, range_decoder,
    },
    testing::env_conformance::{
        EnvConformanceOptions, EnvConformanceResult, create_env_conformance,
    },
};
use tempfile::TempDir;
use tokio::time::{sleep, timeout};

#[tokio::test]
async fn local_execution_environment_conformance() -> EnvConformanceResult {
    let options = EnvConformanceOptions {
        list_files: true,
        symlinks: cfg!(unix),
        ..EnvConformanceOptions::default()
    };
    #[cfg(windows)]
    let options = {
        let mut options = options;
        let bash = ["ProgramFiles", "ProgramFiles(x86)"]
            .into_iter()
            .filter_map(std::env::var_os)
            .map(|path| std::path::PathBuf::from(path).join(r"Git\bin\bash.exe"))
            .find(|path| path.is_file())
            .map_or_else(
                || "bash.exe".to_owned(),
                |path| path.to_string_lossy().into_owned(),
            );
        options.shell[0] = bash;
        options
    };
    for case in create_env_conformance(options) {
        let dir = TempDir::new()?;
        let env = LocalExecutionEnv::with_options(LocalEnvOptions {
            cwd: dir.path().to_string_lossy().into_owned(),
            watch: WatchOptions {
                poll_interval_ms: Some(40),
                ..WatchOptions::default()
            },
            ..LocalEnvOptions::default()
        });
        timeout(Duration::from_secs(40), case.run(&env))
            .await?
            .map_err(|error| format!("{}: {error}", case.name))?;
    }
    Ok(())
}

#[test]
fn every_decoder_split_preserves_interior_bom_and_invalid_sequences() {
    let bytes = b"\xef\xbb\xbfA\xf0\x9f\x92\xa9\xef\xbb\xbf\xe2\x82\n\xf4\x90\x80\x80\xc3";
    for chunk_size in 1..=bytes.len() {
        let mut stream = StreamDecoder::new();
        let mut range = range_decoder();
        let mut stream_text = String::new();
        let mut range_text = String::new();
        for chunk in bytes.chunks(chunk_size) {
            stream_text.push_str(&stream.decode(Some(chunk)));
            range_text.push_str(&range.decode(Some(chunk)));
        }
        stream_text.push_str(&stream.decode(None));
        range_text.push_str(&range.decode(None));
        assert_eq!(
            stream_text,
            "A\u{1f4a9}\u{feff}\u{fffd}\n\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}"
        );
        assert_eq!(
            range_text.strip_prefix('\u{feff}'),
            Some(stream_text.as_str())
        );
    }
}

#[test]
fn line_scan_offsets_exclude_ending_lf_across_all_chunk_boundaries() -> EnvConformanceResult {
    let bytes = b"\xef\xbb\xbfA\n\xe2\x82\n\xef\xbb\xbfb\n";
    let cases = [
        (
            0,
            Some(1),
            LineScan {
                newlines: 3,
                start: 0,
                end: 4,
                first_line_end: 4,
                last_line_start: 0,
                selected_bytes: 1,
                first_line_bytes: 1,
            },
        ),
        (
            1,
            Some(3),
            LineScan {
                newlines: 3,
                start: 5,
                end: 12,
                first_line_end: 7,
                last_line_start: 8,
                selected_bytes: 8,
                first_line_bytes: 3,
            },
        ),
        (
            2,
            Some(3),
            LineScan {
                newlines: 3,
                start: 8,
                end: 12,
                first_line_end: 12,
                last_line_start: 8,
                selected_bytes: 4,
                first_line_bytes: 4,
            },
        ),
        (
            2,
            Some(7),
            LineScan {
                newlines: 3,
                start: 8,
                end: 13,
                first_line_end: 12,
                last_line_start: 13,
                selected_bytes: 5,
                first_line_bytes: 4,
            },
        ),
        (
            4,
            None,
            LineScan {
                newlines: 3,
                start: 13,
                end: 13,
                first_line_end: 13,
                last_line_start: 13,
                selected_bytes: 0,
                first_line_bytes: 0,
            },
        ),
    ];
    for (start, end, expected) in cases {
        for chunk_size in 1..=bytes.len() {
            let mut scanner = LineScanner::new(start, end)?;
            for chunk in bytes.chunks(chunk_size) {
                scanner.push(chunk);
            }
            assert_eq!(
                scanner.finish(),
                expected,
                "{start}..{end:?} chunks of {chunk_size}"
            );
        }
    }
    assert_eq!(
        LineScanner::new(0, None)?.finish(),
        LineScan {
            newlines: 0,
            start: 0,
            end: 0,
            first_line_end: 0,
            last_line_start: 0,
            selected_bytes: 0,
            first_line_bytes: 0
        }
    );
    Ok(())
}

#[tokio::test]
async fn text_lines_are_strict_lf_and_expose_termination() -> EnvConformanceResult {
    let dir = TempDir::new()?;
    let env = LocalExecutionEnv::new(dir.path().to_string_lossy());
    let context = &Context::background();
    let mut bytes = b"\xef\xbb\xbfA\r\n\n".to_vec();
    bytes.resize(65_535, b'x');
    bytes.extend_from_slice(b"\xef\xbb\xbf\xc3\xa9\n\xf0\x9f");
    env.write_file("sample", &bytes, context).await?;
    assert!(
        env.read_text_file("sample", context)
            .await?
            .starts_with('\u{feff}')
    );
    assert_eq!(
        env.read_text_lines("sample", Some(2), context).await?,
        ["A\r", ""]
    );
    assert!(
        env.read_text_lines("missing", Some(0), context)
            .await?
            .is_empty()
    );
    let mut reader = env.open_text_line_reader("sample", context).await?;
    assert_eq!(
        reader.read_line(context).await?,
        Some(TextLine {
            text: "A\r".into(),
            terminated: true
        })
    );
    assert_eq!(
        reader.read_line(context).await?,
        Some(TextLine {
            text: String::new(),
            terminated: true
        })
    );
    let (aborted, cancel) = context.with_cancel();
    cancel.cancel("aborted read");
    assert_eq!(
        reader
            .read_line(&aborted)
            .await
            .err()
            .map(|error| error.code),
        Some(FileErrorCode::Aborted)
    );
    let long = reader
        .read_line(context)
        .await?
        .ok_or("missing long line")?;
    assert!(long.terminated && long.text.starts_with('x') && long.text.ends_with("\u{feff}é"));
    assert_eq!(long.text.len(), 65_535 - 7 + 5);
    assert_eq!(
        reader.read_line(context).await?,
        Some(TextLine {
            text: "\u{fffd}".into(),
            terminated: false
        })
    );
    assert_eq!(reader.read_line(context).await?, None);
    reader.close(context).await;
    assert_eq!(
        reader
            .read_line(context)
            .await
            .err()
            .map(|error| error.code),
        Some(FileErrorCode::Invalid)
    );
    Ok(())
}

#[tokio::test]
async fn local_text_line_reader_handles_large_multi_chunk_line_and_utf8_boundaries()
-> EnvConformanceResult {
    let dir = TempDir::new()?;
    let env = LocalExecutionEnv::new(dir.path().to_string_lossy());
    let context = &Context::background();
    let mut prefix = "x".repeat(65_534);
    prefix.push('🌳');
    let suffix = "y".repeat(1_024 * 1024);
    let long_line = format!("{prefix}{suffix}");
    let content = format!("{long_line}\nshort\ntail");
    env.write_file("large.txt", content.as_bytes(), context)
        .await?;
    let mut reader = env.open_text_line_reader("large.txt", context).await?;
    assert_eq!(
        reader.read_line(context).await?,
        Some(TextLine {
            text: long_line,
            terminated: true,
        })
    );
    assert_eq!(
        reader.read_line(context).await?,
        Some(TextLine {
            text: "short".into(),
            terminated: true,
        })
    );
    assert_eq!(
        reader.read_line(context).await?,
        Some(TextLine {
            text: "tail".into(),
            terminated: false,
        })
    );
    assert_eq!(reader.read_line(context).await?, None);
    reader.close(context).await;
    Ok(())
}

#[tokio::test]
async fn local_file_lifecycle_and_bounds() -> EnvConformanceResult {
    let dir = TempDir::new()?;
    let env = LocalExecutionEnv::new(dir.path().to_string_lossy());
    let context = &Context::background();
    let path = env
        .join_path(&["a", "..", "b", "hello world"], context)
        .await?;
    assert_eq!(Path::new(&path), Path::new("b/hello world"));
    let resolved = env.absolute_path(&path, context).await?;
    #[cfg(unix)]
    assert_eq!(
        env.absolute_path(&format!("file://{}", resolved.replace(' ', "%20")), context)
            .await?,
        resolved
    );
    #[cfg(windows)]
    assert_eq!(
        env.absolute_path(
            &format!(
                "file:///{}",
                resolved.replace('\\', "/").replace(' ', "%20")
            ),
            context
        )
        .await?,
        resolved
    );
    env.write_file(&path, b"abc", context).await?;
    env.append_file(&path, b"def", context).await?;
    assert_eq!(env.read_binary_file(&path, context).await?, b"abcdef");
    env.truncate_file(&path, 3, context).await?;
    env.truncate_file(&path, 5, context).await?;
    env.flush_file(&path, context).await?;
    assert_eq!(env.read_binary_file(&path, context).await?, b"abc\0\0");
    assert_eq!(env.list_dir("b", context).await?.len(), 1);
    assert_eq!(
        env.truncate_file(&path, u64::MAX, context)
            .await
            .err()
            .map(|error| error.code),
        Some(FileErrorCode::Invalid)
    );
    assert_eq!(
        env.flush_file("b", context)
            .await
            .err()
            .map(|error| error.code),
        Some(FileErrorCode::IsDirectory)
    );
    assert!(Path::new(&env.canonical_path(&path, context).await?).is_absolute());
    env.remove(
        "b",
        Some(RemoveOptions {
            recursive: true,
            force: false,
        }),
        context,
    )
    .await?;
    assert!(!env.exists(&path, context).await?);
    env.remove(
        "b",
        Some(RemoveOptions {
            recursive: true,
            force: true,
        }),
        context,
    )
    .await?;
    assert_eq!(
        env.remove("b", None, context)
            .await
            .err()
            .map(|error| error.code),
        Some(FileErrorCode::NotFound)
    );
    let temp = env
        .create_temp_file(
            Some(pi_rs_durable::env::TempFileOptions {
                prefix: Some("native-"),
                suffix: Some(".txt"),
            }),
            context,
        )
        .await?;
    let name = env.file_info(&temp, context).await?.name;
    assert!(name.starts_with("native-") && name.ends_with(".txt"));
    assert!(env.read_binary_file(&temp, context).await?.is_empty());
    let parent = Path::new(&temp)
        .parent()
        .and_then(Path::to_str)
        .ok_or("temporary file has no parent")?;
    env.remove(
        parent,
        Some(RemoveOptions {
            recursive: true,
            force: false,
        }),
        context,
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn polling_watch_detects_rewrites_then_reports_budget_exhaustion_once() -> EnvConformanceResult
{
    let dir = TempDir::new()?;
    let context = &Context::background();
    let env = LocalExecutionEnv::with_options(LocalEnvOptions {
        cwd: dir.path().to_string_lossy().into_owned(),
        watch: WatchOptions {
            mode: Some(WatchMode::Polling),
            poll_interval_ms: Some(20),
            max_directories: Some(1),
        },
        ..LocalEnvOptions::default()
    });
    env.write_file("tree/file", b"one", context).await?;
    let (send, mut recv) = tokio::sync::mpsc::unbounded_channel();
    let target = WatchTarget {
        path: "tree".into(),
        recursive: true,
        exclude: None,
    };
    let mut watcher = env
        .watch(
            std::slice::from_ref(&target),
            Box::new(move |event| {
                let _ = send.send(event);
            }),
            context,
        )
        .await?;
    assert_eq!(watcher.mode(), WatchMode::Polling);
    let path = dir.path().join("tree/file");
    let original_times = std::fs::metadata(&path)?;
    std::fs::write(&path, b"two")?;
    std::fs::File::options()
        .write(true)
        .open(&path)?
        .set_modified(original_times.modified()?)?;
    let expected = env.absolute_path("tree/file", context).await?;
    let event = timeout(Duration::from_secs(2), recv.recv()).await?;
    assert!(matches!(event, Some(FileChange::Paths { paths }) if paths.contains(&expected)));
    env.create_dir("tree/too-many", None, context).await?;
    let failure = timeout(Duration::from_secs(2), recv.recv()).await?;
    assert!(
        matches!(failure, Some(FileChange::Failed { error }) if error.code == FileErrorCode::Invalid)
    );
    assert!(
        timeout(Duration::from_secs(2), recv.recv())
            .await?
            .is_none()
    );
    watcher.stop(context).await;
    let result = env.watch(&[target], Box::new(|_| {}), context).await;
    assert_eq!(
        result.err().map(|error| error.code),
        Some(FileErrorCode::Invalid)
    );
    sleep(Duration::from_millis(25)).await;
    Ok(())
}

#[cfg(unix)]
mod processes {
    use std::collections::BTreeMap;

    use pi_rs_durable::env::{
        ExecutionError, ExecutionErrorCode, Shell, ShellCommand, ShellExecOptions, ShellOutputInfo,
        ShellOutputStream, ShellSpillOptions,
    };
    use tokio::sync::oneshot;

    use super::*;

    #[tokio::test]
    async fn exec_decodes_streams_independently_and_reports_signals() -> EnvConformanceResult {
        let dir = TempDir::new()?;
        let env = LocalExecutionEnv::new(dir.path().to_string_lossy());
        let context = &Context::background();
        let mut stdout = String::new();
        let mut stderr = String::new();
        let mut callback = |text: &str, _: &Context, info: ShellOutputInfo| {
            assert_eq!(info.skipped, None);
            match info.stream {
                ShellOutputStream::Stdout => &mut stdout,
                ShellOutputStream::Stderr => &mut stderr,
            }
            .push_str(text);
            Ok(())
        };
        let script = r"printf '\357\273'; sleep 0.03; printf '\277\342'; printf '\360\237' >&2; sleep 0.03; printf '\202\254'; printf '\222\251' >&2; printf '\342'; kill -TERM $$";
        let status = env
            .exec(
                ShellCommand::Script(script),
                Some(ShellExecOptions {
                    on_output: Some(&mut callback),
                    ..Default::default()
                }),
                context,
            )
            .await?;
        assert_eq!(stdout, "€\u{fffd}");
        assert_eq!(stderr, "\u{1f4a9}");
        assert_eq!(status.exit_code, 143);
        Ok(())
    }

    #[tokio::test]
    async fn spills_preserve_raw_prefix_and_exact_thresholds_for_success_and_timeout()
    -> EnvConformanceResult {
        let dir = TempDir::new()?;
        let env = LocalExecutionEnv::new(dir.path().to_string_lossy());
        let context = &Context::background();
        let script = r"printf '\357\273\277a\n'; sleep 0.025; printf '\377'";
        let exact = env
            .exec(
                ShellCommand::Script(script),
                Some(ShellExecOptions {
                    spill: Some(ShellSpillOptions {
                        after_bytes: 6,
                        after_lines: 2,
                    }),
                    ..Default::default()
                }),
                context,
            )
            .await?;
        assert_eq!(exact.spill_path, None);
        for interrupted in [false, true] {
            let script = if interrupted {
                format!("{script}; sleep 10")
            } else {
                script.to_owned()
            };
            let result = env
                .exec(
                    ShellCommand::Script(&script),
                    Some(ShellExecOptions {
                        timeout: interrupted.then_some(0.3),
                        spill: Some(if interrupted {
                            ShellSpillOptions {
                                after_bytes: 5,
                                after_lines: 2,
                            }
                        } else {
                            ShellSpillOptions {
                                after_bytes: 20,
                                after_lines: 1,
                            }
                        }),
                        ..Default::default()
                    }),
                    context,
                )
                .await;
            let path = if interrupted {
                let error = result.err().ok_or("timeout unexpectedly succeeded")?;
                assert_eq!(error.code, ExecutionErrorCode::Timeout);
                error.spill_path
            } else {
                result?.spill_path
            }
            .ok_or("missing raw spill")?;
            assert_eq!(
                env.read_binary_file(&path, context).await?,
                b"\xef\xbb\xbfa\n\xff"
            );
            let parent = Path::new(&path)
                .parent()
                .and_then(Path::to_str)
                .ok_or("spill has no parent")?;
            env.remove(
                parent,
                Some(RemoveOptions {
                    recursive: true,
                    force: false,
                }),
                context,
            )
            .await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn shell_callback_errors_and_env_overrides_are_typed() -> EnvConformanceResult {
        let dir = TempDir::new()?;
        let env = LocalExecutionEnv::with_options(LocalEnvOptions {
            cwd: dir.path().to_string_lossy().into_owned(),
            shell_env: BTreeMap::from([
                ("DURABLE_BASE".into(), "base".into()),
                ("DURABLE_OVERRIDE".into(), "wrong".into()),
            ]),
            ..Default::default()
        });
        let context = &Context::background();
        let extra = BTreeMap::from([("DURABLE_OVERRIDE".into(), "right".into())]);
        let script = r#"printf '%s:%s' "${DURABLE_BASE:-none}" "$DURABLE_OVERRIDE""#;
        for inherit in [true, false] {
            let mut text = String::new();
            let mut callback = |output: &str, _: &Context, _: ShellOutputInfo| {
                text.push_str(output);
                Ok(())
            };
            let result = env
                .exec(
                    ShellCommand::Script(script),
                    Some(ShellExecOptions {
                        env: Some(&extra),
                        inherit_env: Some(inherit),
                        on_output: Some(&mut callback),
                        ..Default::default()
                    }),
                    context,
                )
                .await?;
            assert_eq!(result.exit_code, 0);
            assert_eq!(text, if inherit { "base:right" } else { "none:right" });
        }
        let mut callback = |_: &str, _: &Context, _: ShellOutputInfo| {
            Err(ExecutionError::new(
                ExecutionErrorCode::Unknown,
                "callback refused",
            ))
        };
        let result = timeout(
            Duration::from_secs(2),
            env.exec(
                ShellCommand::Script("printf x; sleep 10"),
                Some(ShellExecOptions {
                    on_output: Some(&mut callback),
                    ..Default::default()
                }),
                context,
            ),
        )
        .await?;
        let error = result.err().ok_or("callback failure was dropped")?;
        assert_eq!(error.code, ExecutionErrorCode::CallbackError);
        assert_eq!(error.message, "callback refused");
        for timeout in [-1.0, f64::NAN, f64::INFINITY, 2_147_484.0] {
            let result = env
                .exec(
                    ShellCommand::Script("exit 0"),
                    Some(ShellExecOptions {
                        timeout: Some(timeout),
                        ..Default::default()
                    }),
                    context,
                )
                .await;
            assert_eq!(
                result.err().map(|error| error.code),
                Some(ExecutionErrorCode::Timeout)
            );
        }
        let unavailable = LocalExecutionEnv::with_options(LocalEnvOptions {
            cwd: dir.path().to_string_lossy().into_owned(),
            shell_path: Some(dir.path().join("missing-shell").display().to_string()),
            ..Default::default()
        });
        assert_eq!(
            unavailable
                .exec(ShellCommand::Script("exit 0"), None, context)
                .await
                .err()
                .map(|error| error.code),
            Some(ExecutionErrorCode::ShellUnavailable)
        );
        assert_eq!(
            unavailable
                .exec(ShellCommand::Argv(&["sh", "-c", "exit 4"]), None, context)
                .await?
                .exit_code,
            4
        );
        Ok(())
    }

    #[tokio::test]
    async fn abort_kills_descendants_but_not_other_commands() -> EnvConformanceResult {
        let dir = TempDir::new()?;
        let env = LocalExecutionEnv::new(dir.path().to_string_lossy());
        let context = &Context::background();
        let (aborted, cancel) = context.with_cancel();
        let (ready, wait_ready) = oneshot::channel();
        let mut ready = Some(ready);
        let mut callback = |_: &str, _: &Context, _: ShellOutputInfo| {
            if let Some(ready) = ready.take() {
                let _ = ready.send(());
            }
            Ok(())
        };
        let child = env.exec(
            ShellCommand::Script(
                "(sleep 0.45; printf leaked > aborted-descendant) & printf ready; wait",
            ),
            Some(ShellExecOptions {
                on_output: Some(&mut callback),
                ..Default::default()
            }),
            &aborted,
        );
        let other = env.exec(
            ShellCommand::Script("sleep 0.6; printf okay > unrelated"),
            None,
            context,
        );
        let (result, other, ready) = timeout(Duration::from_secs(3), async {
            tokio::join!(child, other, async {
                wait_ready.await?;
                cancel.cancel("stop this command");
                Ok::<(), oneshot::error::RecvError>(())
            })
        })
        .await?;
        ready?;
        assert_eq!(
            result.err().map(|error| error.code),
            Some(ExecutionErrorCode::Aborted)
        );
        assert_eq!(other?.exit_code, 0);
        assert!(env.exists("unrelated", context).await?);
        assert!(!env.exists("aborted-descendant", context).await?);
        Ok(())
    }

    #[tokio::test]
    async fn cleanup_kills_only_its_environment_and_quiet_inherited_pipes_do_not_block()
    -> EnvConformanceResult {
        let dir = TempDir::new()?;
        let cwd = dir.path().to_string_lossy();
        let env = LocalExecutionEnv::new(cwd.as_ref());
        let other_env = LocalExecutionEnv::new(cwd.as_ref());
        let context = &Context::background();
        let (ready, wait_ready) = oneshot::channel();
        let mut ready = Some(ready);
        let mut callback = |_: &str, _: &Context, _: ShellOutputInfo| {
            if let Some(ready) = ready.take() {
                let _ = ready.send(());
            }
            Ok(())
        };
        let child = env.exec(
            ShellCommand::Script(
                "(sleep 0.45; printf leaked > cleanup-descendant) & printf ready; wait",
            ),
            Some(ShellExecOptions {
                on_output: Some(&mut callback),
                ..Default::default()
            }),
            context,
        );
        let other = other_env.exec(
            ShellCommand::Script("sleep 0.6; printf okay > another-env"),
            None,
            context,
        );
        let (result, other, ready) = timeout(Duration::from_secs(3), async {
            tokio::join!(child, other, async {
                wait_ready.await?;
                env.cleanup(context).await;
                Ok::<(), oneshot::error::RecvError>(())
            })
        })
        .await?;
        ready?;
        assert_ne!(result?.exit_code, 0);
        assert_eq!(other?.exit_code, 0);
        assert!(env.exists("another-env", context).await?);
        assert!(!env.exists("cleanup-descendant", context).await?);
        let result = timeout(
            Duration::from_millis(450),
            env.exec(ShellCommand::Script("sleep 0.8 & exit 0"), None, context),
        )
        .await??;
        assert_eq!(result.exit_code, 0);
        Ok(())
    }

    #[tokio::test]
    async fn post_exit_output_rearms_idle_grace() -> EnvConformanceResult {
        let dir = TempDir::new()?;
        let env = LocalExecutionEnv::new(dir.path().to_string_lossy());
        let context = &Context::background();
        let mut text = String::new();
        let mut callback = |output: &str, _: &Context, _: ShellOutputInfo| {
            text.push_str(output);
            Ok(())
        };
        let script = "(sleep 0.03; printf a; sleep 0.06; printf b; sleep 0.06; printf c) & exit 0";
        let result = env
            .exec(
                ShellCommand::Script(script),
                Some(ShellExecOptions {
                    on_output: Some(&mut callback),
                    ..Default::default()
                }),
                context,
            )
            .await?;
        assert_eq!(result.exit_code, 0);
        assert_eq!(text, "abc");
        Ok(())
    }

    #[tokio::test]
    async fn native_watch_reports_when_a_dangling_symlink_referent_appears() -> EnvConformanceResult
    {
        let dir = TempDir::new()?;
        let env = LocalExecutionEnv::with_options(LocalEnvOptions {
            cwd: dir.path().to_string_lossy().into_owned(),
            watch: WatchOptions {
                mode: Some(WatchMode::Native),
                poll_interval_ms: Some(30),
                ..Default::default()
            },
            ..Default::default()
        });
        let context = &Context::background();
        env.create_dir("config", None, context).await?;
        env.create_dir("data", None, context).await?;
        std::os::unix::fs::symlink("../data/file", dir.path().join("config/file"))?;
        let path = env.absolute_path("config/file", context).await?;
        let (send, mut recv) = tokio::sync::mpsc::unbounded_channel();
        let target = WatchTarget {
            path: "config/file".into(),
            ..Default::default()
        };
        let mut watcher = env
            .watch(
                &[target],
                Box::new(move |event| {
                    let _ = send.send(event);
                }),
                context,
            )
            .await?;
        env.write_file("data/file", b"first", context).await?;
        timeout(Duration::from_secs(3), async {
            while let Some(change) = recv.recv().await {
                if matches!(change, FileChange::Paths { paths } if paths.contains(&path)) {
                    return Ok(());
                }
            }
            Err("watcher never observed the new referent")
        })
        .await??;
        watcher.stop(context).await;
        Ok(())
    }

    #[tokio::test]
    async fn binary_reader_never_waits_for_a_fifo_writer() -> EnvConformanceResult {
        let dir = TempDir::new()?;
        let env = LocalExecutionEnv::new(dir.path().to_string_lossy());
        let context = &Context::background();
        assert_eq!(
            env.exec(ShellCommand::Argv(&["mkfifo", "fifo"]), None, context)
                .await?
                .exit_code,
            0
        );
        // A writer makes a regression fail instead of permanently blocking Tokio's I/O worker.
        let (result, _) = tokio::join!(
            timeout(
                Duration::from_millis(150),
                env.open_binary_reader("fifo", None, context)
            ),
            env.exec(
                ShellCommand::Script("sleep 0.25; test ! -p fifo || printf x > fifo"),
                Some(ShellExecOptions {
                    timeout: Some(0.5),
                    ..Default::default()
                }),
                context
            )
        );
        assert_eq!(
            result?.err().map(|error| error.code),
            Some(FileErrorCode::Invalid)
        );
        Ok(())
    }
}
