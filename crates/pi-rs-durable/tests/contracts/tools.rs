#[cfg(unix)]
use std::time::Duration;
use std::{borrow::Cow, error::Error};

use async_trait::async_trait;
use pi_rs_ai::{InputContent, Tool};
use pi_rs_chord::context::Context;
use pi_rs_durable::{
    env::{FileSystem, LocalExecutionEnv},
    tool_search::{ToolSearchHost, ToolSearchToolInput, create_tool_search_tool},
    tools::{
        ReadToolInput, ReadToolOptions, ToolError, ToolResult, WriteToolInput, create_coding_tools,
        create_edit_tool, create_read_tool, create_write_tool,
        edit_diff::{
            Edit, apply_edits_to_normalized_content, generate_diff_string, generate_unified_patch,
        },
        prepare_edit_arguments, with_file_mutation_queue,
    },
    truncate::{
        DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncationLimit, TruncationOptions, truncate_head,
    },
};
use serde_json::{Value, json};

#[cfg(unix)]
use pi_rs_durable::tools::{
    BashToolInput, BashToolOptions, DiagnosticSeverity, ShellOutput, ShellStructuredOutput,
    TailShellOutput, create_bash_tool,
};

fn text<D>(result: &ToolResult<D>) -> Option<&str> {
    match result.output.as_deref()? {
        [InputContent::Text(text)] => Some(&text.text),
        _ => None,
    }
}

fn test_env() -> std::io::Result<(tempfile::TempDir, LocalExecutionEnv)> {
    let dir = tempfile::tempdir()?;
    let env = LocalExecutionEnv::new(dir.path().to_string_lossy());
    Ok((dir, env))
}

fn read(path: &str, offset: Option<f64>, limit: Option<f64>) -> ReadToolInput {
    ReadToolInput {
        path: path.into(),
        offset,
        limit,
    }
}

fn edit(old: &str, new: &str) -> Edit {
    Edit {
        old_text: old.into(),
        new_text: new.into(),
    }
}

#[tokio::test]
async fn coding_tools_write_then_read_and_edit_bom_crlf() -> Result<(), Box<dyn Error>> {
    let (_dir, env) = test_env()?;
    let context = Context::background();
    let tools = create_coding_tools(ReadToolOptions::default());
    assert_eq!(
        tools.specs().map(|spec| spec.name),
        ["read", "write", "edit", "bash"]
    );
    assert!(
        pi_rs_schema::compile(&tools.write.spec().parameters)?.check(&pi_rs_schema::Value::from(
            json!({"path":"p","content":"x","extra":true})
        ))
    );
    assert_eq!(
        tools
            .read
            .execute(read("missing", None, None), None, &context)
            .await
            .err()
            .map(|e| e.to_string())
            .as_deref(),
        Some("No execution environment is configured")
    );

    let result = tools
        .write
        .execute(
            WriteToolInput {
                path: "@nested/a\u{2007}b.txt".into(),
                content: "\u{feff}alpha\r\nbeta\r\ngamma\r\ndelta\r\n".into(),
            },
            Some(&env),
            &context,
        )
        .await?;
    assert_eq!(
        text(&result),
        Some("Successfully wrote to @nested/a\u{2007}b.txt")
    );
    let path = "nested/a b.txt";
    let out = tools
        .read
        .execute(read(path, Some(2.0), Some(2.0)), Some(&env), &context)
        .await?;
    assert_eq!(text(&out), Some("beta\r\ngamma\r"));
    assert_eq!(out.structured_output, Some(json!("beta\r\ngamma\r")));
    assert_eq!(
        out.diagnostics
            .as_deref()
            .and_then(|d| d.first())
            .map(|d| d.message.as_str()),
        Some("2 more lines in file. Use offset=4 to continue.")
    );

    let args = json!({"path":path,"edits":"{\"oldText\":\"alpha\",\"newText\":\"ALPHA\"}","oldText":"gamma","newText":"GAMMA"});
    let fixed = prepare_edit_arguments(&args);
    assert!(args["edits"].is_string());
    assert_eq!(
        fixed,
        json!({"path":path,"edits":[{"oldText":"alpha","newText":"ALPHA"},{"oldText":"gamma","newText":"GAMMA"}]})
    );
    let result = tools.edit.execute_json(&args, Some(&env), &context).await?;
    assert_eq!(
        text(&result),
        Some("Successfully replaced 2 block(s) in nested/a b.txt.")
    );
    let details = result.details.ok_or("edit did not provide details")?;
    assert_eq!(details.first_changed_line, Some(1));
    assert_eq!(
        details.diff,
        "-1 alpha\n+1 ALPHA\n 2 beta\n-3 gamma\n+3 GAMMA\n 4 delta"
    );
    assert_eq!(
        details.patch,
        "--- nested/a b.txt\n+++ nested/a b.txt\n@@ -1,4 +1,4 @@\n-alpha\n+ALPHA\n beta\n-gamma\n+GAMMA\n delta\n"
    );
    assert_eq!(
        env.read_text_file(path, &context).await?,
        "\u{feff}ALPHA\r\nbeta\r\nGAMMA\r\ndelta\r\n"
    );
    Ok(())
}

#[tokio::test]
async fn read_path_falls_back_to_mac_file_spellings() -> Result<(), Box<dyn Error>> {
    let (_dir, env) = test_env()?;
    let ctx = Context::background();
    let tool = create_read_tool(ReadToolOptions::default());
    for (stored, requested) in [
        ("Screen 9.00\u{202f}PM.png", "@Screen 9.00 PM.png"),
        ("e\u{301}tat.txt", "état.txt"),
        ("e\u{301}t\u{2019}al.txt", "ét'al.txt"),
    ] {
        env.write_file(stored, b"hello", &ctx).await?;
        assert_eq!(
            text(
                &tool
                    .execute(read(requested, None, None), Some(&env), &ctx)
                    .await?
            ),
            Some("hello")
        );
    }
    env.write_file("e\u{301}tat.txt", b"fallback", &ctx).await?;
    env.write_file("état.txt", b"exact", &ctx).await?;
    assert_eq!(
        text(
            &tool
                .execute(read("état.txt", None, None), Some(&env), &ctx)
                .await?
        ),
        Some("exact")
    );
    Ok(())
}

#[tokio::test]
async fn read_counts_continuations_and_cuts_utf8_at_boundary() -> Result<(), Box<dyn Error>> {
    let (_dir, env) = test_env()?;
    let ctx = Context::background();
    let tool = create_read_tool(ReadToolOptions::default());
    for (path, bytes) in [
        ("many", "x\n".repeat(DEFAULT_MAX_LINES + 1)),
        ("exact", "x\n".repeat(DEFAULT_MAX_LINES)),
        ("long", format!("{}\nafter\n", "€".repeat(25_000))),
    ] {
        env.write_file(path, bytes.as_bytes(), &ctx).await?;
    }
    let many = tool
        .execute(read("many", None, None), Some(&env), &ctx)
        .await?;
    let truncation = many
        .details
        .and_then(|d| d.truncation)
        .ok_or("read dropped its truncation details")?;
    assert_eq!(truncation.truncated_by, Some(TruncationLimit::Lines));
    assert_eq!(truncation.output_lines, DEFAULT_MAX_LINES);
    assert_eq!(truncation.total_lines, DEFAULT_MAX_LINES + 1);
    let exact = tool
        .execute(read("exact", None, None), Some(&env), &ctx)
        .await?;
    assert!(exact.details.is_none());
    let long = tool
        .execute(read("long", None, None), Some(&env), &ctx)
        .await?;
    let kept = DEFAULT_MAX_BYTES / '€'.len_utf8();
    assert_eq!(text(&long), Some("€".repeat(kept).as_str()));
    let details = serde_json::to_value(long.details.ok_or("missing details")?)?;
    assert_eq!(details["truncation"]["outputBytes"], kept * '€'.len_utf8());
    assert_eq!(details["truncation"]["firstLineExceedsLimit"], true);
    assert!(details["truncation"].get("content").is_none());
    Ok(())
}

#[tokio::test]
async fn bounded_read_matches_whole_decoding_and_js_slice_edges() -> Result<(), Box<dyn Error>> {
    let (_dir, env) = test_env()?;
    let ctx = Context::background();
    let tool = create_read_tool(ReadToolOptions::default());
    let files: [&[u8]; 5] = [
        b"",
        b"\xef\xbb\xbf",
        b"\xef\xbb\xbfa\n\xef\xbb\xbfb\r\nc\n\xe2\x82\n\xff",
        b"a\n\nb\n",
        b"\n\n\n",
    ];
    for bytes in files {
        env.write_file("f", bytes, &ctx).await?;
        let decoded = String::from_utf8_lossy(bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes));
        let lines: Vec<_> = decoded.split('\n').collect();
        for offset in [
            None,
            Some(-4.0),
            Some(2.0),
            Some(2.5),
            Some(1e20),
            Some(f64::NAN),
        ] {
            for limit in [
                None,
                Some(0.0),
                Some(1.5),
                Some(-3.0),
                Some(-1e20),
                Some(f64::NAN),
            ] {
                let actual = tool
                    .execute_with_model(read("f", offset, limit), Some(&env), &ctx, || async {
                        Err(ToolError::Message("text read resolved a model".into()))
                    })
                    .await;
                let start = (offset.unwrap_or(1.0_f64) - 1.0).max(0.0);
                if start >= lines.len() as f64 {
                    assert!(
                        actual
                            .err()
                            .is_some_and(|error| error.to_string().contains("beyond end of file"))
                    );
                    continue;
                }
                let end = match limit {
                    None => lines.len(),
                    Some(value) if value.is_nan() => 0,
                    Some(value) => {
                        let relative = (start + value).min(lines.len() as f64).trunc();
                        if relative < 0.0 {
                            (lines.len() as f64 + relative).max(0.0) as usize
                        } else {
                            relative as usize
                        }
                    }
                };
                let selection = lines
                    .get(start as usize..end)
                    .unwrap_or_default()
                    .join("\n");
                let reference = truncate_head(&selection, TruncationOptions::default());
                assert_eq!(
                    actual?.structured_output,
                    Some(json!(reference.content)),
                    "bytes={bytes:?}, offset={offset:?}, limit={limit:?}"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn fuzzy_edits_normalize_only_touched_lines_and_reject_ambiguous_matches()
-> Result<(), Box<dyn Error>> {
    let original = "leave ‘exact’  \nＡ — ‘target’\u{2002}word  \nuntouched ‘quote’ \nlast";
    let result = apply_edits_to_normalized_content(
        original,
        &[edit("A - 'target' word", "changed"), edit("last", "LAST")],
        "f",
    )?;
    assert_eq!(result.base_content, original);
    assert_eq!(
        result.new_content,
        "leave ‘exact’  \nchanged\nuntouched ‘quote’ \nLAST"
    );
    for (original, edits, expected) in [
        (
            "alpha alpha",
            vec![edit("alpha", "x")],
            "Found 2 occurrences",
        ),
        (
            "alpha ‘x’\nalpha 'x'",
            vec![edit("alpha 'x'", "x")],
            "Found 2 occurrences",
        ),
        (
            "alpha beta",
            vec![edit("alpha beta", "x"), edit("beta", "y")],
            "overlap",
        ),
        (
            "alpha",
            vec![edit("alpha", "beta"), edit("beta", "BETA")],
            "Could not find edits[1]",
        ),
        ("alpha", vec![edit("alpha", "alpha")], "No changes made"),
        (
            "alpha",
            vec![edit("", "alpha")],
            "oldText must not be empty",
        ),
    ] {
        let error = apply_edits_to_normalized_content(original, &edits, "f")
            .err()
            .ok_or("ambiguous edit succeeded")?;
        assert!(error.to_string().contains(expected), "{error}");
    }
    assert_eq!(
        generate_unified_patch("f", "a", "b", 4),
        "--- f\n+++ f\n@@ -1,1 +1,1 @@\n-a\n\\ No newline at end of file\n+b\n\\ No newline at end of file\n"
    );
    assert_eq!(
        generate_unified_patch("f", "", "b\n", 4),
        "--- f\n+++ f\n@@ -0,0 +1,1 @@\n+b\n"
    );
    let display = generate_diff_string(
        "zero\none\ntwo\nthree\nfour\nfive",
        "zero\nONE\ntwo\nthree\nfour\nFIVE",
        1,
    );
    assert_eq!(
        display.diff,
        " 1 zero\n-2 one\n+2 ONE\n 3 two\n   ...\n 5 four\n-6 five\n+6 FIVE"
    );
    assert_eq!(display.first_changed_line, Some(2));
    Ok(())
}

#[tokio::test]
async fn rejected_edit_never_writes_partial_replacements() -> Result<(), Box<dyn Error>> {
    let (_dir, env) = test_env()?;
    let ctx = Context::background();
    env.write_file("f", b"one\ntwo\nthree\n", &ctx).await?;
    let tool = create_edit_tool();
    let args = json!({"path":"f","edits":[{"oldText":"one\ntwo","newText":"A"},{"oldText":"two\nthree","newText":"B"}]});
    assert!(
        tool.execute_json(&args, Some(&env), &ctx)
            .await
            .err()
            .is_some_and(|e| e.to_string().contains("overlap"))
    );
    assert_eq!(env.read_text_file("f", &ctx).await?, "one\ntwo\nthree\n");
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn mutation_queue_serializes_a_missing_file_through_symlinked_directories()
-> Result<(), Box<dyn Error>> {
    use tokio::{pin, sync::Notify, time::timeout};
    let (dir, first_env) = test_env()?;
    let second_env = LocalExecutionEnv::new(dir.path().to_string_lossy());
    std::fs::create_dir(dir.path().join("real"))?;
    std::os::unix::fs::symlink("real", dir.path().join("link"))?;
    let bg = Context::background();
    let (ctx, cancel) = bg.with_cancel();
    let entered = Notify::new();
    let release = Notify::new();
    let first = with_file_mutation_queue(
        &first_env,
        "link/new\\file",
        async || {
            entered.notify_one();
            release.notified().await;
            first_env
                .write_file("link/new\\file", b"first", &bg)
                .await?;
            assert!(ctx.is_aborted());
            Err::<(), _>(ToolError::Aborted)
        },
        &ctx,
    );
    let second = async {
        entered.notified().await;
        cancel.cancel("interrupted");
        // A separate file can be written while this file is still in the queue.
        timeout(
            Duration::from_secs(2),
            create_write_tool().execute(
                WriteToolInput {
                    path: "real/other".into(),
                    content: "other".into(),
                },
                Some(&second_env),
                &bg,
            ),
        )
        .await??;
        let write = create_write_tool();
        let waiter = write.execute(
            WriteToolInput {
                path: "real/new\\file".into(),
                content: "second".into(),
            },
            Some(&second_env),
            &bg,
        );
        pin!(waiter);
        assert!(
            timeout(Duration::from_millis(40), &mut waiter)
                .await
                .is_err()
        );
        release.notify_one();
        waiter.await?;
        Ok::<(), Box<dyn Error>>(())
    };
    let (first, second) = tokio::join!(first, second);
    assert!(matches!(first, Err(ToolError::Aborted)));
    second?;
    assert_eq!(
        first_env.read_text_file("link/new\\file", &bg).await?,
        "second"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn shell_streams_exit_code_spills_and_timeouts() -> Result<(), Box<dyn Error>> {
    let (_dir, env) = test_env()?;
    let ctx = Context::background();
    let bash = create_bash_tool(BashToolOptions {
        command_prefix: Some("message=hello"),
        prepare: None,
    });
    let mut output = TailShellOutput::default();
    let result = bash
        .execute(
            BashToolInput {
                command: "printf '%s' \"$message\"; printf ' stderr' >&2; exit 7".into(),
                timeout: None,
            },
            Some(&env),
            &mut output,
            &ctx,
        )
        .await?;
    let retained: ShellStructuredOutput =
        serde_json::from_value(result.structured_output.ok_or("no shell value")?)?;
    assert_eq!(retained.exit_code, 7);
    assert!(retained.output.contains("hello"));
    assert!(retained.output.contains("stderr"));
    assert_eq!(result.is_error, Some(true));
    assert!(result.output.is_none());
    assert_eq!(
        result
            .diagnostics
            .as_deref()
            .and_then(|d| d.first())
            .map(|d| d.severity),
        Some(DiagnosticSeverity::Error)
    );
    assert!(output.diagnostics().is_empty());

    let mut output = TailShellOutput::default();
    let result = bash
        .execute(
            BashToolInput {
                command: format!(
                    "for ((i=1; i<={}; i++)); do printf 'line-%s\\n' \"$i\"; done",
                    DEFAULT_MAX_LINES + 10
                ),
                timeout: None,
            },
            Some(&env),
            &mut output,
            &ctx,
        )
        .await?;
    let retained: ShellStructuredOutput =
        serde_json::from_value(result.structured_output.ok_or("no shell value")?)?;
    assert!(retained.truncated);
    assert!(retained.output.starts_with("line-11\n"));
    let spill = retained.full_output_path.ok_or("no spill")?;
    assert_eq!(
        output.diagnostics()[0].message,
        format!("Full output: {spill}")
    );
    let full = std::fs::read_to_string(&spill)?;
    std::fs::remove_file(&spill)?;
    assert!(full.starts_with("line-1\nline-2\n"));
    assert_eq!(full.lines().count(), DEFAULT_MAX_LINES + 10);

    let mut output = TailShellOutput::default();
    let error = bash
        .execute(
            BashToolInput {
                command: "printf 'before timeout'; sleep 5".into(),
                timeout: Some(0.2),
            },
            Some(&env),
            &mut output,
            &ctx,
        )
        .await
        .err()
        .ok_or("timeout succeeded")?;
    assert_eq!(error.to_string(), "Command timed out after 0.2 seconds");
    assert_eq!(output.retained_output().text, "before timeout");
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn powershell_falls_back_only_on_spawn_and_passes_script_verbatim()
-> Result<(), Box<dyn Error>> {
    use pi_rs_durable::tools::{PowerShellToolOptions, create_powershell_tool};
    use std::os::unix::fs::PermissionsExt;
    let (dir, env) = test_env()?;
    let binary = dir.path().join("powershell-fixture");
    std::fs::write(&binary, b"#!/bin/sh\nprintf '%s\\n' \"$@\"\nexit 3\n")?;
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700))?;
    let binary = binary.to_str().ok_or("fixture path not UTF-8")?;
    let programs = [
        "pi-uninstalled-shell-first",
        binary,
        "pi-uninstalled-shell-last",
    ];
    let shell = create_powershell_tool(PowerShellToolOptions {
        programs: Some(&programs),
        command_prefix: Some("$x=1"),
        prepare: None,
    });
    let mut output = TailShellOutput::default();
    let ctx = Context::background();
    let result = shell
        .execute(
            BashToolInput {
                command: "Write-Output 'héllo'".into(),
                timeout: None,
            },
            Some(&env),
            &mut output,
            &ctx,
        )
        .await?;
    assert_eq!(result.is_error, Some(true));
    let value: ShellStructuredOutput =
        serde_json::from_value(result.structured_output.ok_or("no shell value")?)?;
    assert_eq!(value.exit_code, 3);
    assert_eq!(
        value.output,
        "-NoProfile\n-NonInteractive\n-ExecutionPolicy\nBypass\n-Command\ntry { [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 } catch {}\n$x=1\nWrite-Output 'héllo'\n"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn bash_prepare_changes_command_working_directory_and_inheritance()
-> Result<(), Box<dyn Error>> {
    use pi_rs_durable::{
        env::{ExecutionEnv, LocalEnvOptions},
        tools::{BashExecution, BashPrepare},
    };

    struct Prepare(String);

    #[async_trait]
    impl BashPrepare for Prepare {
        async fn prepare(
            &self,
            execution: &mut BashExecution,
            env: &dyn ExecutionEnv,
            _: &mut dyn ShellOutput,
            ctx: &Context,
        ) -> Result<(), ToolError> {
            assert_eq!(execution.cwd, env.cwd());
            assert!(ctx.abort_signal().is_some());
            execution.cwd.clone_from(&self.0);
            execution.inherit_env = false;
            execution
                .env
                .insert("PI_PREPARED_ONLY".into(), "explicit".into());
            execution.command.push_str("\n: > marker\nprintf '%s:%s:%s' \"$prefix\" \"${PI_INHERITED_ONLY-}\" \"$PI_PREPARED_ONLY\"");
            Ok(())
        }
    }

    let dir = tempfile::tempdir()?;
    let cwd = dir.path().join("prepared");
    std::fs::create_dir(&cwd)?;
    let env = LocalExecutionEnv::with_options(LocalEnvOptions {
        cwd: dir.path().to_string_lossy().into_owned(),
        shell_env: [("PI_INHERITED_ONLY".into(), "parent".into())].into(),
        ..LocalEnvOptions::default()
    });
    let prepare = Prepare(cwd.to_string_lossy().into_owned());
    let bash = create_bash_tool(BashToolOptions {
        command_prefix: Some("prefix=ready"),
        prepare: Some(&prepare),
    });
    let (ctx, _cancel) = Context::background().with_cancel();
    let mut output = TailShellOutput::default();
    let result = bash
        .execute(
            BashToolInput {
                command: ":".into(),
                timeout: None,
            },
            Some(&env),
            &mut output,
            &ctx,
        )
        .await?;
    assert_eq!(
        result.structured_output.ok_or("missing shell output")?["output"],
        "ready::explicit"
    );
    assert!(cwd.join("marker").is_file());
    Ok(())
}

#[cfg(unix)]
#[test]
fn tail_consumer_accounts_for_output_skipped_before_the_retained_window()
-> Result<(), Box<dyn Error>> {
    use pi_rs_durable::env::{ShellOutputSkip, ShellOutputWindow};

    let window = ShellOutputWindow {
        max_bytes: 10,
        max_lines: 2,
        min_interval_ms: 100,
        bytes_per_second: 1024,
    };
    let mut output = TailShellOutput::new(Some(window));
    assert_eq!(output.window(), Some(window));
    output.output(
        "older\nlast\n",
        Some(ShellOutputSkip {
            bytes: 18,
            newlines: 3,
            ends_with_newline: true,
        }),
    )?;
    let retained = output.retained_output();
    assert!(retained.truncated);
    assert_eq!(retained.text, "last\n");
    Ok(())
}

struct Registry(Vec<Tool>);

#[async_trait]
impl ToolSearchHost for Registry {
    async fn deferred_tools(&self, _: &Context) -> Result<Cow<'_, [Tool]>, ToolError> {
        Ok(Cow::Borrowed(&self.0))
    }
}

#[tokio::test]
async fn tool_search_follows_pointer_refs_and_returns_an_atomic_load_request()
-> Result<(), Box<dyn Error>> {
    let mut registry = Registry(vec![
        Tool {
            name: "ledger_inspect".into(),
            description: "Inspect records\r\nDetailed help".into(),
            constrained_sampling: None,
            parameters: json!({
                "properties": {"options":{"$ref":"#/$defs/comp%20~1~0"}, "alias":{"$ref":"#/$defs/comp%20~1~0"}},
                "$defs": {
                    "comp /~": {"properties":{"mode":{"description":"reconciliation"},"cycle":{"$ref":"#/$defs/comp%20~1~0"}}},
                    "unreachable": {"description":"forecast"}
                }
            }),
        },
        Tool {
            name: "repo_list".into(),
            description: "List source repositories".into(),
            constrained_sampling: None,
            parameters: Value::Bool(true),
        },
    ]);
    let tool = create_tool_search_tool();
    let ctx = Context::background();
    assert!(tool.model_only() && tool.replay_safe());
    for (query, limit, expected) in [
        ("\u{feff}\u{a0}", None, "query must not be empty"),
        ("records", Some(0.5), "limit must be a positive integer"),
    ] {
        let actual = tool
            .execute(
                ToolSearchToolInput {
                    query: query.into(),
                    limit,
                },
                &registry,
                &ctx,
            )
            .await
            .err()
            .ok_or("invalid discovery input succeeded")?;
        assert_eq!(actual.to_string(), expected);
    }
    let unrelated = tool
        .execute(
            ToolSearchToolInput {
                query: "forecast".into(),
                limit: None,
            },
            &registry,
            &ctx,
        )
        .await?;
    assert!(unrelated.control.is_none());
    let result = tool
        .execute(
            ToolSearchToolInput {
                query: "reconciliation".into(),
                limit: Some(1.0),
            },
            &registry,
            &ctx,
        )
        .await?;
    assert_eq!(
        text(&result),
        Some(
            "Loaded 1 tool. They are available from your next call:\n- ledger_inspect: Inspect records"
        )
    );
    assert_eq!(
        serde_json::to_value(&result)?["control"],
        json!({"addTools":["ledger_inspect"]})
    );
    let loaded = result.control.ok_or("missing load request")?.add_tools;
    assert_eq!(
        result.details.ok_or("missing discovery details")?.loaded,
        loaded
    );
    registry.0.retain(|tool| !loaded.contains(&tool.name));
    let later = tool
        .execute(
            ToolSearchToolInput {
                query: "reconciliation".into(),
                limit: None,
            },
            &registry,
            &ctx,
        )
        .await?;
    assert_eq!(text(&later), Some("No matching tools found."));
    Ok(())
}
