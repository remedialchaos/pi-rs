//! File operation tracking shared by compaction and branch summarization.
//!
//! Ports upstream `utils.ts` `createFileOps`, `extractFileOpsFromMessage`,
//! `computeFileLists` and `formatFileOperations`.

use std::collections::BTreeSet;

use serde_json::Value;

use super::{CompactionDetails, MessageValue, role_of};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FileLists {
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

/// Tracks paths read, written, and edited by tool calls.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FileOpsTracker {
    read: BTreeSet<String>,
    written: BTreeSet<String>,
    edited: BTreeSet<String>,
}

impl FileOpsTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_read(&mut self, path: impl Into<String>) {
        self.read.insert(path.into());
    }

    pub fn record_write(&mut self, path: impl Into<String>) {
        self.written.insert(path.into());
    }

    pub fn record_edit(&mut self, path: impl Into<String>) {
        self.edited.insert(path.into());
    }

    pub fn read(&self) -> &BTreeSet<String> {
        &self.read
    }

    pub fn written(&self) -> &BTreeSet<String> {
        &self.written
    }

    pub fn edited(&self) -> &BTreeSet<String> {
        &self.edited
    }

    /// Collect prior file lists from a previous pi-generated compaction or
    /// branch summary so cumulative tracking survives iterative summaries.
    pub fn merge_previous_details(&mut self, details: &CompactionDetails) {
        for path in &details.read_files {
            self.record_read(path.clone());
        }
        for path in &details.modified_files {
            self.record_edit(path.clone());
        }
    }

    /// Extract tool-call file operations from an assistant message, or from
    /// nested calls recorded on a codemode tool result.
    pub fn extract_from_message(&mut self, message: &MessageValue) {
        match role_of(message) {
            "toolResult" => {
                if let Some(calls) = message
                    .get("nestedCalls")
                    .and_then(|nested| nested.get("calls"))
                    .and_then(Value::as_array)
                {
                    for call in calls {
                        self.add_call(
                            call.get("name").and_then(Value::as_str).unwrap_or(""),
                            call.get("arguments"),
                        );
                    }
                }
            }
            "assistant" => {
                if let Some(blocks) = message.get("content").and_then(Value::as_array) {
                    for block in blocks {
                        if block.get("type").and_then(Value::as_str) == Some("toolCall") {
                            self.add_call(
                                block.get("name").and_then(Value::as_str).unwrap_or(""),
                                block.get("arguments"),
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn add_call(&mut self, name: &str, arguments: Option<&Value>) {
        let Some(path) = arguments
            .and_then(|args| args.get("path"))
            .and_then(Value::as_str)
        else {
            return;
        };
        match name {
            "read" => self.record_read(path),
            "write" => self.record_write(path),
            "edit" => self.record_edit(path),
            _ => {}
        }
    }

    /// Read-only files plus modified files, both sorted like upstream.
    pub fn compute_file_lists(&self) -> FileLists {
        let modified: BTreeSet<String> = self
            .edited
            .iter()
            .chain(self.written.iter())
            .cloned()
            .collect();
        let read_files = self
            .read
            .iter()
            .filter(|path| !modified.contains(*path))
            .cloned()
            .collect();
        FileLists {
            read_files,
            modified_files: modified.into_iter().collect(),
        }
    }

    /// XML fragments appended to a summary, or an empty string when no files
    /// were touched.
    pub fn to_summary_string(&self) -> String {
        let lists = self.compute_file_lists();
        format_file_operations(&lists.read_files, &lists.modified_files)
    }
}

/// Upstream `formatFileOperations`: ready-to-append XML with a leading blank
/// line, or an empty string when there is nothing to report.
pub fn format_file_operations(read_files: &[String], modified_files: &[String]) -> String {
    let mut sections = Vec::new();
    if !read_files.is_empty() {
        sections.push(format!(
            "<read-files>\n{}\n</read-files>",
            read_files.join("\n")
        ));
    }
    if !modified_files.is_empty() {
        sections.push(format!(
            "<modified-files>\n{}\n</modified-files>",
            modified_files.join("\n")
        ));
    }
    if sections.is_empty() {
        String::new()
    } else {
        format!("\n\n{}", sections.join("\n\n"))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn records_read_write_edit_and_deduplicates() {
        let mut tracker = FileOpsTracker::new();
        tracker.record_read("a.ts");
        tracker.record_read("a.ts");
        tracker.record_write("b.ts");
        tracker.record_edit("c.ts");
        assert_eq!(tracker.read().len(), 1);
        assert_eq!(tracker.written().len(), 1);
        assert_eq!(tracker.edited().len(), 1);
        assert_eq!(
            tracker.compute_file_lists(),
            FileLists {
                read_files: vec!["a.ts".to_string()],
                modified_files: vec!["b.ts".to_string(), "c.ts".to_string()],
            }
        );
    }

    #[test]
    fn modified_files_are_removed_from_read_only_list() {
        let mut tracker = FileOpsTracker::new();
        tracker.record_read("a.ts");
        tracker.record_read("b.ts");
        tracker.record_edit("a.ts");
        let lists = tracker.compute_file_lists();
        assert_eq!(lists.read_files, vec!["b.ts".to_string()]);
        assert_eq!(lists.modified_files, vec!["a.ts".to_string()]);
    }

    #[test]
    fn extracts_tool_calls_from_assistant_messages() {
        let message = json!({
            "role": "assistant",
            "content": [
                {"type": "text", "text": "working"},
                {"type": "toolCall", "id": "1", "name": "read", "arguments": {"path": "src/main.rs"}},
                {"type": "toolCall", "id": "2", "name": "edit", "arguments": {"path": "src/lib.rs"}},
                {"type": "toolCall", "id": "3", "name": "write", "arguments": {"path": "src/new.rs"}},
                {"type": "toolCall", "id": "4", "name": "bash", "arguments": {"command": "ls"}}
            ]
        });
        let mut tracker = FileOpsTracker::new();
        tracker.extract_from_message(&message);
        assert_eq!(tracker.compute_file_lists().modified_files.len(), 2);
        assert_eq!(
            tracker.compute_file_lists().read_files,
            vec!["src/main.rs".to_string()]
        );
    }

    #[test]
    fn extracts_nested_calls_from_tool_results() {
        let result = json!({
            "role": "toolResult",
            "toolCallId": "codemode-1",
            "toolName": "codemode",
            "content": [],
            "nestedCalls": {
                "calls": [
                    {"id": "1", "name": "read", "arguments": {"path": "a.ts"}, "status": "ok"},
                    {"id": "2", "name": "edit", "arguments": {"path": "b.ts", "edits": []}, "status": "ok"},
                    {"id": "3", "name": "write", "argumentsBytes": 40000, "status": "ok"}
                ],
                "complete": false
            }
        });
        let mut tracker = FileOpsTracker::new();
        tracker.extract_from_message(&result);
        assert_eq!(
            tracker.compute_file_lists(),
            FileLists {
                read_files: vec!["a.ts".to_string()],
                modified_files: vec!["b.ts".to_string()],
            }
        );
    }

    #[test]
    fn summary_string_uses_upstream_xml_and_handles_empty() {
        let mut tracker = FileOpsTracker::new();
        assert_eq!(tracker.to_summary_string(), "");
        tracker.record_read("a.ts");
        tracker.record_write("b.ts");
        assert_eq!(
            tracker.to_summary_string(),
            "\n\n<read-files>\na.ts\n</read-files>\n\n<modified-files>\nb.ts\n</modified-files>"
        );
    }

    #[test]
    fn merges_previous_compaction_details() {
        let mut tracker = FileOpsTracker::new();
        tracker.merge_previous_details(&CompactionDetails {
            read_files: vec!["old.rs".to_string()],
            modified_files: vec!["changed.rs".to_string()],
        });
        tracker.record_edit("changed.rs");
        let lists = tracker.compute_file_lists();
        assert_eq!(lists.read_files, vec!["old.rs".to_string()]);
        assert_eq!(lists.modified_files, vec!["changed.rs".to_string()]);
    }
}
