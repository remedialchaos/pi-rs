use super::{Invocation, LIVE_DOC};
use crate::{
    documents::resolve_address,
    session::{CommitOptions, CommitPersistence, Error, Result},
    tools::{DiagnosticSeverity, ToolDiagnostic, ToolResult},
    truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES},
};
use pi_rs_chord::{
    context::Context,
    delta::Segment,
    output::{BoundedOutput, OutputBuffer, OutputLimits, OutputSkip, Retain},
};
use serde_json::{Value, json};

pub(super) struct Progress {
    buffer: OutputBuffer,
    details: Option<Value>,
    diagnostics: Vec<ToolDiagnostic>,
    retain: Retain,
}
impl Default for Progress {
    fn default() -> Self {
        Self::new(OutputLimits {
            max_bytes: DEFAULT_MAX_BYTES,
            max_lines: DEFAULT_MAX_LINES,
            retain: Retain::Head,
        })
    }
}
impl Progress {
    pub(super) fn new(limits: OutputLimits) -> Self {
        Self {
            buffer: OutputBuffer::new(limits),
            details: None,
            diagnostics: Vec::new(),
            retain: limits.retain,
        }
    }
    pub(super) fn finish(&mut self, result: &mut ToolResult) {
        self.buffer.end();
        if result.output.is_none() {
            let output = self.buffer.snapshot();
            let text = String::from_utf16_lossy(&output.text.0);
            result.output = Some(if text.is_empty() {
                Vec::new()
            } else {
                vec![pi_rs_ai::types::InputContent::Text(text.into())]
            });
            if output.dropped_bytes > 0 {
                let kept = if self.retain == Retain::Head {
                    "beginning"
                } else {
                    "end"
                };
                self.diagnostics.push(ToolDiagnostic::new(
                    DiagnosticSeverity::Warn,
                    Some("truncated"),
                    format!(
                        "Output truncated to its {kept}: {} lines, {} bytes dropped",
                        output.dropped_lines, output.dropped_bytes
                    ),
                ));
            }
        }
        if result.details.is_none() {
            result.details = self.details.take();
        }
        let mut diagnostics = std::mem::take(&mut self.diagnostics);
        diagnostics.extend(result.diagnostics.take().unwrap_or_default());
        result.diagnostics = Some(diagnostics);
    }
}
impl Invocation {
    pub fn output(&self, text: &str, skipped: Option<OutputSkip>) -> Result<()> {
        self.progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .buffer
            .push_text(&text.into(), skipped)?;
        self.progress_notice.notify_one();
        Ok(())
    }
    pub fn output_bytes(&self, bytes: &[u8], skipped: Option<OutputSkip>) -> Result<()> {
        self.progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .buffer
            .push_bytes(bytes, skipped)?;
        self.progress_notice.notify_one();
        Ok(())
    }
    pub fn diagnostic(&self, diagnostic: ToolDiagnostic) {
        self.progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .diagnostics
            .push(diagnostic);
        self.progress_notice.notify_one();
    }
    pub fn retained_output(&self) -> BoundedOutput {
        self.progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .buffer
            .snapshot()
    }
    pub async fn details(&self, value: Value, ctx: &Context) -> Result<()> {
        self.progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .details = Some(value);
        self.publish_tool_progress(ctx).await
    }
    pub(super) async fn publish_tool_progress(&self, ctx: &Context) -> Result<()> {
        self.write_tool_progress(ctx, false).await
    }

    pub(super) async fn write_tool_progress(&self, ctx: &Context, closing: bool) -> Result<()> {
        if self.record.input["progress"] == false {
            return Ok(());
        }
        let (output, details, diagnostics) = {
            let mut progress = self.progress.lock().unwrap_or_else(|e| e.into_inner());
            (
                progress.buffer.snapshot(),
                progress.details.clone(),
                progress.diagnostics.clone(),
            )
        };
        let addr = resolve_address(&LIVE_DOC, Some(self.record.conversation_id), None)?;
        self.harness
            .session
            .commit_with(
                async |tx| {
                    let current = tx.task(self.record.id).await?;
                    if current.is_none_or(|t| t.state != self.record.state || t.abort_requested) {
                        return if closing {
                            Ok(())
                        } else {
                            Err(Error::invalid("Tool invocation ended"))
                        };
                    }
                    let doc = tx.doc(&LIVE_DOC, &addr, None).await?;
                    let state = serde_json::to_value(doc.value()?.to_json())?;
                    let key = if self.record.input["kind"] == "nested" {
                        "nestedTools"
                    } else {
                        "tools"
                    };
                    if let Some(i) = state[key]
                        .as_array()
                        .and_then(|items| items.iter().position(|s| s["taskId"] == self.record.id))
                    {
                        let mut slot = state[key][i].clone();
                        slot["status"] = json!("running");
                        if !output.text.0.is_empty() {
                            slot["output"] = json!(String::from_utf16_lossy(&output.text.0));
                        }
                        if output.dropped_bytes > 0 {
                            slot["droppedBytes"] = json!(output.dropped_bytes);
                            slot["droppedLines"] = json!(output.dropped_lines);
                        }
                        if let Some(details) = details {
                            slot["details"] = details;
                        }
                        if !diagnostics.is_empty() {
                            slot["diagnostics"] = json!(diagnostics);
                        }
                        doc.set(&[Segment::Key(key.into()), Segment::Index(i)], slot.into())?;
                    }
                    Ok(())
                },
                ctx,
                Default::default(),
                CommitOptions {
                    persistence: CommitPersistence::Deferred { within: None },
                },
            )
            .await
    }
}
