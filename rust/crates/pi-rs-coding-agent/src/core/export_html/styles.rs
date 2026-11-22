//! CSS for the self-contained HTML export.
//!
//! Trimmed from `packages/coding-agent/src/core/export-html/template.css`: every
//! selector used by the server-rendered document is kept, along with the theme
//! custom properties and the whitespace rules that preserve tool output.

/// Dark theme variables. Upstream derives these from the resolved interactive
/// theme; the export keeps a fixed, self-contained palette instead.
const DARK_VARS: &str = r#"
      --text: #d4d4d4;
      --dim: #6b6b6b;
      --muted: #8a8a8a;
      --accent: #7aa2f7;
      --success: #4ec9b0;
      --error: #f14c4c;
      --warning: #e8a838;
      --border: #3a3a3a;
      --borderAccent: #569cd6;
      --selectedBg: #2a2d2e;
      --hover: #2a2d2e;
      --userMessageBg: #343541;
      --userMessageText: #e5e5e5;
      --thinkingText: #8a8a8a;
      --toolPendingBg: #2a2d2e;
      --toolSuccessBg: #1f2d24;
      --toolErrorBg: #352125;
      --toolOutput: #bdbdbd;
      --toolDiffAdded: #4ec9b0;
      --toolDiffRemoved: #f14c4c;
      --toolDiffContext: #8a8a8a;
      --customMessageBg: #2b2b3a;
      --customMessageLabel: #c586c0;
      --customMessageText: #c8c8c8;
      --mdHeading: #dcdcaa;
      --mdLink: #7aa2f7;
      --mdCode: #ce9178;
      --mdQuote: #a8a8a8;
      --mdQuoteBorder: #569cd6;
      --mdListBullet: #c586c0;
      --mdHr: #3a3a3a;
      --mdCodeBlockBorder: #3a3a3a;
      --syntaxComment: #6a9955;
      --syntaxKeyword: #c586c0;
      --syntaxNumber: #b5cea8;
      --syntaxString: #ce9178;
      --syntaxFunction: #dcdcaa;
      --syntaxType: #4ec9b0;
      --syntaxVariable: #9cdcfe;
      --syntaxOperator: #d4d4d4;
      --syntaxPunctuation: #a8a8a8;
      --body-bg: rgb(24, 24, 30);
      --container-bg: rgb(30, 30, 36);
      --info-bg: rgb(60, 55, 40);"#;

/// Light theme variables, selected when the requested theme name contains
/// `light` (matching upstream's theme-name conventions).
const LIGHT_VARS: &str = r#"
      --text: #1e1e1e;
      --dim: #767676;
      --muted: #666666;
      --accent: #0b62c4;
      --success: #0a7b52;
      --error: #c01818;
      --warning: #8a5b00;
      --border: #d0d0d0;
      --borderAccent: #0b62c4;
      --selectedBg: #e8eef7;
      --hover: #eff3f8;
      --userMessageBg: #eef1f7;
      --userMessageText: #1e1e1e;
      --thinkingText: #666666;
      --toolPendingBg: #eef1f7;
      --toolSuccessBg: #eaf6ee;
      --toolErrorBg: #fdecee;
      --toolOutput: #333333;
      --toolDiffAdded: #0a7b52;
      --toolDiffRemoved: #c01818;
      --toolDiffContext: #666666;
      --customMessageBg: #f2eef8;
      --customMessageLabel: #7a2c8c;
      --customMessageText: #333333;
      --mdHeading: #8a5b00;
      --mdLink: #0b62c4;
      --mdCode: #a31515;
      --mdQuote: #555555;
      --mdQuoteBorder: #0b62c4;
      --mdListBullet: #7a2c8c;
      --mdHr: #d0d0d0;
      --mdCodeBlockBorder: #d0d0d0;
      --syntaxComment: #008000;
      --syntaxKeyword: #af00db;
      --syntaxNumber: #098658;
      --syntaxString: #a31515;
      --syntaxFunction: #795e26;
      --syntaxType: #267f99;
      --syntaxVariable: #001080;
      --syntaxOperator: #1e1e1e;
      --syntaxPunctuation: #555555;
      --body-bg: rgb(246, 246, 248);
      --container-bg: rgb(238, 238, 242);
      --info-bg: rgb(255, 248, 220);"#;

const STYLESHEET: &str = r#"
    * { margin: 0; padding: 0; box-sizing: border-box; }

    :root {
      --line-height: 18px; /* 12px font * 1.5 */
    }

    body {
      font-family: ui-monospace, 'Cascadia Code', 'Source Code Pro', Menlo, Consolas, 'DejaVu Sans Mono', monospace;
      font-size: 12px;
      line-height: var(--line-height);
      color: var(--text);
      background: var(--body-bg);
    }

    #app { display: flex; min-height: 100vh; }

    /* Session tree */
    #sidebar {
      width: 280px;
      min-width: 240px;
      max-width: 840px;
      background: var(--container-bg);
      flex-shrink: 0;
      position: sticky;
      top: 0;
      height: 100vh;
      overflow: auto;
      border-right: 1px solid var(--dim);
      padding: 8px 0;
    }

    .sidebar-heading {
      padding: 4px 12px 8px;
      font-weight: bold;
      color: var(--borderAccent);
      border-bottom: 1px solid var(--dim);
      margin-bottom: 4px;
    }

    .tree-node {
      padding: 0 8px;
      font-size: 11px;
      line-height: 13px;
      white-space: nowrap;
    }

    .tree-node.in-path { background: color-mix(in srgb, var(--accent) 10%, transparent); }
    .tree-node:not(.in-path) { opacity: 0.5; }
    .tree-prefix { color: var(--muted); font-family: monospace; white-space: pre; flex-shrink: 0; }
    .tree-marker { color: var(--accent); flex-shrink: 0; }
    .tree-content { color: var(--text); }
    .tree-role-user { color: var(--accent); }
    .tree-role-skill { color: var(--customMessageLabel); }
    .tree-role-assistant { color: var(--success); }
    .tree-role-tool { color: var(--muted); }
    .tree-muted { color: var(--muted); }
    .tree-error { color: var(--error); }
    .tree-compaction { color: var(--borderAccent); }
    .tree-branch-summary { color: var(--warning); }
    .tree-custom-message { color: var(--customMessageLabel); }

    /* Main content */
    #content {
      flex: 1;
      min-width: 0;
      overflow-y: auto;
      padding: var(--line-height) calc(var(--line-height) * 2);
      display: flex;
      flex-direction: column;
      align-items: center;
    }

    #content > * { width: 100%; max-width: 800px; }

    /* Header */
    .header {
      background: var(--container-bg);
      border-radius: 4px;
      padding: var(--line-height);
      margin-bottom: var(--line-height);
    }

    .header h1 {
      font-size: 12px;
      font-weight: bold;
      color: var(--borderAccent);
      margin-bottom: var(--line-height);
    }

    .header-info { display: flex; flex-direction: column; font-size: 11px; }
    .info-item { color: var(--dim); display: flex; align-items: baseline; }
    .info-label { font-weight: 600; margin-right: 8px; min-width: 100px; }
    .info-value { color: var(--text); flex: 1; }

    /* Messages */
    #messages { display: flex; flex-direction: column; gap: var(--line-height); }
    .message-timestamp { font-size: 10px; color: var(--dim); opacity: 0.8; }

    .user-message {
      background: var(--userMessageBg);
      color: var(--userMessageText);
      padding: var(--line-height);
      border-radius: 4px;
      position: relative;
    }

    .assistant-message { padding: 0; position: relative; }
    .assistant-text { padding: var(--line-height); padding-bottom: 0; }
    .message-timestamp + .assistant-text,
    .message-timestamp + .thinking-block,
    .thinking-block + .assistant-text { padding-top: 0; }

    .thinking-text {
      padding: var(--line-height);
      color: var(--thinkingText);
      font-style: italic;
      white-space: pre-wrap;
    }

    .thinking-collapsed {
      display: none;
      padding: var(--line-height);
      color: var(--thinkingText);
      font-style: italic;
    }

    .thinking-block[open] .thinking-text { display: block; }
    .thinking-block[open] .thinking-collapsed { display: none; }

    /* Tool execution */
    .tool-execution { padding: var(--line-height); border-radius: 4px; }
    .tool-execution + .tool-execution { margin-top: var(--line-height); }
    .assistant-text + .tool-execution { margin-top: var(--line-height); }
    .tool-execution.pending { background: var(--toolPendingBg); }
    .tool-execution.success { background: var(--toolSuccessBg); }
    .tool-execution.error { background: var(--toolErrorBg); }
    .tool-header, .tool-name { font-weight: bold; }
    .tool-path { color: var(--accent); word-break: break-all; }
    .line-numbers { color: var(--warning); }
    .line-count { color: var(--dim); }

    .tool-command {
      font-weight: bold;
      white-space: pre-wrap;
      word-wrap: break-word;
      overflow-wrap: break-word;
      word-break: break-word;
    }

    .tool-output {
      margin-top: var(--line-height);
      color: var(--toolOutput);
      word-wrap: break-word;
      overflow-wrap: break-word;
      word-break: break-word;
      font-family: inherit;
      overflow-x: auto;
    }

    /* Preserve whitespace for plain-text tool output lines, not for the
       container itself: an indented source template must not add blank space. */
    .tool-output > div,
    .output-preview > div,
    .output-full > div { margin: 0; padding: 0; line-height: var(--line-height); }

    .tool-output > div:not(.output-preview):not(.output-full),
    .output-preview > div:not(.expand-hint),
    .output-full > div:not(.expand-hint) { white-space: pre-wrap; }

    .tool-output pre { margin: 0; padding: 0; font-family: inherit; color: inherit; white-space: pre-wrap; }
    .tool-output code { padding: 0; background: none; color: var(--text); }
    .ansi-line { white-space: pre; }

    .tool-image { max-width: 100%; max-height: 500px; border-radius: 4px; margin: var(--line-height) 0; }
    .expand-hint { color: var(--toolOutput); }

    /* Diff */
    .tool-diff { font-size: 11px; overflow-x: auto; white-space: pre; }
    .diff-added { color: var(--toolDiffAdded); }
    .diff-removed { color: var(--toolDiffRemoved); }
    .diff-context { color: var(--toolDiffContext); }

    /* Model change / compaction / system prompt */
    .model-change { padding: 0 var(--line-height); color: var(--dim); font-size: 11px; }
    .model-name { color: var(--borderAccent); font-weight: bold; }
    .compaction { background: var(--customMessageBg); border-radius: 4px; padding: var(--line-height); }
    .compaction-label { color: var(--customMessageLabel); font-weight: bold; }
    .compaction-collapsed { color: var(--customMessageText); }
    .compaction-content { display: none; color: var(--customMessageText); white-space: pre-wrap; margin-top: var(--line-height); }
    .compaction[open] .compaction-collapsed { display: none; }
    .compaction[open] .compaction-content { display: block; }

    .system-prompt {
      background: var(--customMessageBg);
      padding: var(--line-height);
      border-radius: 4px;
      margin-bottom: var(--line-height);
    }

    .system-prompt-header { font-weight: bold; color: var(--customMessageLabel); }
    .system-prompt-preview, .system-prompt-full {
      color: var(--customMessageText);
      white-space: pre-wrap;
      word-wrap: break-word;
      font-size: 11px;
      margin-top: var(--line-height);
    }

    .system-prompt-expand-hint { color: var(--muted); font-style: italic; margin-top: 4px; }
    .system-prompt:not(.expandable) .system-prompt-full { display: block; margin-top: 4px; }
    .system-prompt.expandable .system-prompt-full { display: none; }
    .system-prompt.expandable[open] .system-prompt-full { display: block; }
    .system-prompt.expandable[open] .system-prompt-preview,
    .system-prompt.expandable[open] .system-prompt-expand-hint { display: none; }
    .tool-item > summary { cursor: pointer; }
    .tool-item:not(:has(> summary)) .tool-params-hint { display: none; }
    .system-prompt-note { font-size: 10px; font-style: italic; color: var(--muted); margin-top: 4px; }
    .system-entry > .message-timestamp { padding-left: var(--line-height); margin-bottom: 4px; }
    .system-entry > :last-child { margin-bottom: 0; }

    /* Tools list */
    .tools-list { background: var(--customMessageBg); padding: var(--line-height); border-radius: 4px; margin-bottom: var(--line-height); }
    .tools-header { font-weight: bold; color: var(--customMessageLabel); margin-bottom: var(--line-height); }
    .tool-item { font-size: 11px; }
    .tool-item-name { font-weight: bold; color: var(--text); }
    .tool-item-desc { color: var(--dim); }
    .tool-param { margin-bottom: 4px; font-size: 11px; }
    .tool-param-name { font-weight: bold; color: var(--text); }
    .tool-param-type { color: var(--dim); font-style: italic; }
    .tool-param-required { color: var(--warning); font-size: 10px; }
    .tool-param-optional { color: var(--dim); font-size: 10px; }
    .tool-param-desc { color: var(--dim); margin-left: 8px; }

    /* Custom messages / skills / branch summary */
    .hook-message { background: var(--customMessageBg); color: var(--customMessageText); padding: var(--line-height); border-radius: 4px; }
    .hook-type { color: var(--customMessageLabel); font-weight: bold; }

    .skill-invocation { background: var(--customMessageBg); border-radius: 4px; padding: var(--line-height); }
    .skill-invocation-label { color: var(--customMessageLabel); font-weight: bold; }
    .skill-invocation-collapsed { color: var(--customMessageText); }
    .skill-invocation-content { display: none; color: var(--customMessageText); margin-top: var(--line-height); }
    .skill-invocation[open] .skill-invocation-collapsed { display: none; }
    .skill-invocation[open] .skill-invocation-content { display: block; }
    .skill-invocation + .user-message { margin-top: var(--line-height); }

    .branch-summary { background: var(--customMessageBg); padding: var(--line-height); border-radius: 4px; }
    .branch-summary-header { font-weight: bold; color: var(--borderAccent); }

    /* Errors / images */
    .error-text { color: var(--error); padding: 0 var(--line-height); }
    .tool-error { color: var(--error); }
    .message-images { margin-bottom: 12px; }
    .message-image { max-width: 100%; max-height: 400px; border-radius: 4px; margin: var(--line-height) 0; }

    /* Markdown content */
    .markdown-content h1, .markdown-content h2, .markdown-content h3,
    .markdown-content h4, .markdown-content h5, .markdown-content h6 {
      color: var(--mdHeading);
      margin: var(--line-height) 0 0 0;
      font-weight: bold;
      font-size: 1em;
    }

    .markdown-content p { margin: 0; }
    .markdown-content p + p { margin-top: var(--line-height); }
    .markdown-content a { color: var(--mdLink); text-decoration: underline; }
    .markdown-content code { background: rgba(128, 128, 128, 0.2); color: var(--mdCode); padding: 0 4px; border-radius: 3px; font-family: inherit; }
    .markdown-content pre { background: transparent; margin: var(--line-height) 0; overflow-x: auto; }
    .markdown-content pre code { display: block; background: none; color: var(--text); }
    .markdown-content blockquote { border-left: 3px solid var(--mdQuoteBorder); padding-left: var(--line-height); margin: var(--line-height) 0; color: var(--mdQuote); font-style: italic; }
    .markdown-content ul, .markdown-content ol { margin: var(--line-height) 0; padding-left: calc(var(--line-height) * 2); }
    .markdown-content li { margin: 0; }
    .markdown-content li::marker { color: var(--mdListBullet); }
    .markdown-content hr { border: none; border-top: 1px solid var(--mdHr); margin: var(--line-height) 0; }
    .markdown-content table { border-collapse: collapse; margin: 0.5em 0; width: 100%; }
    .markdown-content th, .markdown-content td { border: 1px solid var(--mdCodeBlockBorder); padding: 6px 10px; text-align: left; }
    .markdown-content th { background: rgba(128, 128, 128, 0.1); font-weight: bold; }
    .markdown-content img { max-width: 100%; border-radius: 4px; }

    /* Syntax highlighting (hljs classes emitted by syntect) */
    .hljs { background: transparent; color: var(--text); }
    .hljs-comment, .hljs-quote { color: var(--syntaxComment); }
    .hljs-keyword, .hljs-selector-tag { color: var(--syntaxKeyword); }
    .hljs-number, .hljs-literal { color: var(--syntaxNumber); }
    .hljs-string, .hljs-doctag { color: var(--syntaxString); }
    .hljs-function, .hljs-title, .hljs-title.function_, .hljs-section, .hljs-name { color: var(--syntaxFunction); }
    .hljs-type, .hljs-class, .hljs-title.class_, .hljs-built_in { color: var(--syntaxType); }
    .hljs-attr, .hljs-variable, .hljs-variable.language_, .hljs-params, .hljs-property { color: var(--syntaxVariable); }
    .hljs-meta, .hljs-meta .hljs-keyword, .hljs-meta .hljs-string { color: var(--syntaxKeyword); }
    .hljs-operator { color: var(--syntaxOperator); }
    .hljs-punctuation { color: var(--syntaxPunctuation); }
    .hljs-subst { color: var(--text); }

    .footer { margin-top: 48px; padding: 20px; text-align: center; color: var(--dim); font-size: 10px; }

    @media (max-width: 900px) {
      #sidebar { position: fixed; left: 0; width: min(280px, 100vw); min-width: min(280px, 100vw); max-width: min(280px, 100vw); z-index: 10; display: none; }
      #content { padding: var(--line-height) 16px; }
      #content > * { max-width: 100%; }
    }

    @media print {
      #sidebar { display: none !important; }
      body { background: white; color: black; }
      #content { max-width: none; }
    }
"#;

/// Return the full inline stylesheet for a requested theme name.
pub fn document_css(theme_name: Option<&str>) -> String {
    let is_light = theme_name
        .map(|name| name.to_ascii_lowercase().contains("light"))
        .unwrap_or(false);
    let vars = if is_light { LIGHT_VARS } else { DARK_VARS };
    format!("    :root {{{vars}\n    }}{STYLESHEET}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn css_is_self_contained_and_preserves_whitespace() {
        let css = document_css(None);
        assert!(!css.contains("@import"));
        assert!(!css.contains("url("));
        assert!(css.contains("--body-bg"));
        assert!(css.contains(".ansi-line { white-space: pre; }"));
        assert!(css.contains("white-space: pre-wrap;"));
    }

    #[test]
    fn light_theme_switches_palette() {
        assert!(document_css(Some("github-light")).contains("--text: #1e1e1e;"));
        assert!(!document_css(Some("dracula")).contains("--text: #1e1e1e;"));
    }
}
