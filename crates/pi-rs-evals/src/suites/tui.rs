use super::*;
use serde_json::json;
use std::{process::Stdio, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(super) fn case() -> Case {
    Case::new(
        "tui",
        "Customize the interactive context footer",
        "replaces numeric context usage with a progress bar",
    )
}
pub(super) fn input() -> PiCodingAgentInput {
    steps("Change the Pi footer replacing the built-in context info, e.g. `42.2%/272k (auto)`, with a ten-cell progress bar to make it show ████░░░░░░ 42% instead. Clamp percentages to the 0-100 range. Just replace the context. Otherwise leave everything else the same.".into(),None)
}

const FIXTURES: [(u64, &str); 3] = [
    (114784, "████░░░░░░ 42%"),
    (176800, "███████░░░ 65%"),
    (326400, "██████████ 100%"),
];

pub(super) async fn judge(pi: &Path, ws: &EvalWorkspace) -> EvalResult<bool> {
    // Keep the render inputs separate from any files the evaluated agent produced.
    let fixtures = EvalWorkspace::new(&BTreeMap::new())?;
    let ansi = regex::Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]")
        .map_err(|e| EvalError::message(e.to_string()))?;
    let builtin = regex::Regex::new(r"\d+(?:\.\d+)?%/272k(?: \(auto\))?")
        .map_err(|e| EvalError::message(e.to_string()))?;
    for (index, (tokens, bar)) in FIXTURES.iter().enumerate() {
        let now = chrono::Utc::now().to_rfc3339();
        let path = fixtures.workspace.join(format!("context-{index}.jsonl"));
        let header = json!({"type":"session","version":3,"id":uuid::Uuid::new_v4().to_string(),"timestamp":now,"cwd":ws.workspace});
        let message = json!({"role":"assistant","content":[{"type":"text","text":"Context usage fixture"}],"api":"openai-responses","provider":"openai","model":"gpt-5.4","usage":{"input":tokens,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":tokens,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"stop","timestamp":0});
        let entry = json!({"type":"message","id":"00000001","parentId":null,"timestamp":now,"message":message});
        std::fs::write(&path, format!("{header}\n{entry}\n"))?;
        if index == 0
            && !builtin.is_match(&ansi.replace_all(&render(pi, ws, &path, false).await?, ""))
        {
            return Err(EvalError::message(
                "TUI probe did not observe the built-in 272k context footer",
            ));
        }
        let raw = render(pi, ws, &path, true).await?;
        let text = ansi.replace_all(&raw, "");
        let matching = text.lines().any(|line| {
            line.contains(bar)
                && !builtin.is_match(line)
                && FIXTURES
                    .iter()
                    .all(|(_, other)| other == bar || !line.contains(other))
        });
        if !matching {
            return Ok(false);
        }
    }
    Ok(true)
}

// util-linux script supplies the same PTY transport as an interactive session in the OCI runner.
// The shell program is fixed; paths are passed solely as quoted environment values.
async fn render(
    pi: &Path,
    ws: &EvalWorkspace,
    session: &Path,
    extensions: bool,
) -> EvalResult<String> {
    let vars = BTreeMap::from([
        ("PI_PROBE_BIN".into(), pi.to_string_lossy().into_owned()),
        (
            "PI_PROBE_SESSION".into(),
            session.to_string_lossy().into_owned(),
        ),
        ("OPENAI_API_KEY".into(), "fixture-only".into()),
        ("TERM".into(), "xterm-256color".into()),
    ]);
    let mut command = ws.command(Path::new("script"), &vars)?;
    let script = if extensions {
        r#"stty cols 100 rows 30 && exec "$PI_PROBE_BIN" --offline --no-refresh --approve --tui-mode regular --provider openai --model gpt-5.4 --session "$PI_PROBE_SESSION""#
    } else {
        r#"stty cols 100 rows 30 && exec "$PI_PROBE_BIN" --no-extensions --offline --no-refresh --approve --tui-mode regular --provider openai --model gpt-5.4 --session "$PI_PROBE_SESSION""#
    };
    let mut child = command
        .args(["-q", "-c", script, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| EvalError::message("Missing TUI input"))?;
    let mut out = child
        .stdout
        .take()
        .ok_or_else(|| EvalError::message("Missing TUI output"))?;
    let mut captured = Vec::new();
    let mut buf = [0u8; 8192];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let painted = captured.windows(8).any(|s| s == b"\x1b[?2026l");
        let limit = if painted {
            deadline.min(tokio::time::Instant::now() + Duration::from_millis(350))
        } else {
            deadline
        };
        let read = tokio::time::timeout_at(limit, out.read(&mut buf)).await;
        let Ok(n) = read else {
            break;
        };
        let n = n?;
        if n == 0 {
            break;
        }
        captured.extend_from_slice(&buf[..n]);
        if captured.len() > 2 * 1024 * 1024 {
            return Err(EvalError::message("TUI render exceeded capture limit"));
        }
    }
    let _ = input.write_all(b"\x04\x04").await;
    drop(input);
    if tokio::time::timeout(Duration::from_secs(1), child.wait())
        .await
        .is_err()
    {
        child.kill().await?;
    }
    Ok(String::from_utf8_lossy(&captured).into_owned())
}
