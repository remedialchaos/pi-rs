//! Rust port of `packages/coding-agent/src/core/prompt-templates.ts`.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use super::skills::{
    CONFIG_DIR_NAME, ResourceDiagnostic, SourceInfo, create_synthetic_source_info,
    extract_frontmatter, is_path_within, resolve_path_default,
};

#[derive(Debug, Clone, Serialize)]
pub struct PromptTemplate {
    pub name: String,
    pub description: String,
    #[serde(rename = "argumentHint", skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
    pub content: String,
    pub source_info: SourceInfo,
    pub file_path: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct LoadPromptTemplatesOptions {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub prompt_paths: Vec<PathBuf>,
    pub include_defaults: bool,
}

#[derive(Debug, Clone, Default)]
pub struct LoadPromptTemplatesResult {
    pub templates: Vec<PromptTemplate>,
    pub diagnostics: Vec<ResourceDiagnostic>,
}

/// Parse a command argument string with bash-style quoting.
pub fn parse_command_args(args_string: &str) -> Vec<String> {
    let chars: Vec<char> = args_string.chars().collect();
    let mut args = Vec::new();
    let mut current = String::new();
    let mut has_token = false;
    let mut in_quote: Option<char> = None;
    let mut quote_start = 0usize;
    let mut i = 0usize;

    while i < chars.len() {
        let ch = chars[i];
        if let Some(quote) = in_quote {
            if ch == '\\' && chars.get(i + 1) == Some(&quote) {
                current.push(quote);
                i += 2;
            } else if ch == quote {
                in_quote = None;
                i += 1;
            } else {
                current.push(ch);
                i += 1;
            }
        } else if (ch == '"' || ch == '\'') && !has_token {
            in_quote = Some(ch);
            quote_start = i;
            has_token = true;
            i += 1;
        } else if ch.is_whitespace() {
            if has_token {
                args.push(std::mem::take(&mut current));
                has_token = false;
            }
            i += 1;
        } else {
            current.push(ch);
            has_token = true;
            i += 1;
        }

        if in_quote.is_some() && i >= chars.len() {
            // Unclosed quote: re-read the opening quote as literal text.
            in_quote = None;
            current.clear();
            current.push(chars[quote_start]);
            i = quote_start + 1;
        }
    }

    if has_token {
        args.push(current);
    }
    args
}

fn js_parse_int(text: &str) -> Option<i64> {
    if text.is_empty() || !text.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    Some(text.parse::<i64>().unwrap_or(i64::MAX))
}

fn indexed_arg(args: &[String], index: i64) -> Option<&String> {
    usize::try_from(index)
        .ok()
        .and_then(|index| args.get(index))
}

/// Substitute `$N`, `$@`/`$ARGUMENTS`, `${N:-default}` and `${@:N[:L]}`.
pub fn substitute_args(content: &str, args: &[String]) -> String {
    let all_args = args.join(" ");
    let chars: Vec<char> = content.chars().collect();
    let mut output = String::new();
    let mut i = 0usize;

    while i < chars.len() {
        if chars[i] != '$' {
            output.push(chars[i]);
            i += 1;
            continue;
        }

        let mut matched = false;
        if chars.get(i + 1) == Some(&'{')
            && let Some(relative_end) = chars[i + 2..].iter().position(|ch| *ch == '}')
        {
            let end = i + 2 + relative_end;
            let inner: String = chars[i + 2..end].iter().collect();

            // ${N:-default}, ${@:-default}, ${ARGUMENTS:-default}
            if let Some((target, default)) = inner.split_once(":-")
                && (target == "@" || target == "ARGUMENTS" || js_parse_int(target).is_some())
            {
                let value = if target == "@" || target == "ARGUMENTS" {
                    Some(all_args.as_str())
                } else {
                    let index = js_parse_int(target).and_then(|value| value.checked_sub(1));
                    index
                        .and_then(|index| indexed_arg(args, index))
                        .map(String::as_str)
                };
                output.push_str(value.filter(|value| !value.is_empty()).unwrap_or(default));
                i = end + 1;
                matched = true;
            }
            // ${@:N} and ${@:N:L}
            if !matched && let Some(rest) = inner.strip_prefix("@:") {
                let (start_text, length_text) = rest.split_once(':').unwrap_or((rest, ""));
                // Upstream clamps negative starts (including `${@:0}`) to zero.
                let start = js_parse_int(start_text)
                    .map_or(0, |value| value.saturating_sub(1))
                    .max(0);
                let length = if length_text.is_empty() {
                    None
                } else {
                    js_parse_int(length_text)
                };
                if length.is_some() || length_text.is_empty() {
                    let slice_start = usize::try_from(start).unwrap_or(usize::MAX).min(args.len());
                    let slice_end = length
                        .and_then(|length| usize::try_from(length).ok())
                        .map_or(args.len(), |length| {
                            slice_start.saturating_add(length).min(args.len())
                        });
                    output.push_str(&args[slice_start.min(slice_end)..slice_end].join(" "));
                    i = end + 1;
                    matched = true;
                }
            }
        }
        if matched {
            continue;
        }

        // $ARGUMENTS, $@, $N
        let simple: String = chars[i + 1..]
            .iter()
            .take_while(|ch| ch.is_ascii_alphanumeric() || **ch == '@')
            .collect();
        if simple == "ARGUMENTS" || simple == "@" {
            output.push_str(&all_args);
            i += 1 + simple.len();
            continue;
        }
        if !simple.is_empty() && simple.chars().all(|ch| ch.is_ascii_digit()) {
            let index = js_parse_int(&simple).and_then(|value| value.checked_sub(1));
            output.push_str(
                index
                    .and_then(|index| indexed_arg(args, index))
                    .map_or("", String::as_str),
            );
            i += 1 + simple.len();
            continue;
        }
        output.push('$');
        i += 1;
    }
    output
}

fn load_template_from_file(
    file_path: &Path,
    source_info: SourceInfo,
) -> (Option<PromptTemplate>, Vec<ResourceDiagnostic>) {
    let mut diagnostics = Vec::new();
    let raw_content = match fs::read_to_string(file_path) {
        Ok(content) => content,
        Err(error) => {
            diagnostics.push(ResourceDiagnostic::warning(
                format!("{error}"),
                Some(file_path),
            ));
            return (None, diagnostics);
        }
    };
    let (frontmatter, body) = match extract_frontmatter(&raw_content) {
        Ok(parsed) => parsed,
        Err(message) => {
            diagnostics.push(ResourceDiagnostic::warning(message, Some(file_path)));
            return (None, diagnostics);
        }
    };

    let file_name = file_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = file_name
        .strip_suffix(".md")
        .unwrap_or(&file_name)
        .to_string();

    let mut description = frontmatter
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if description.is_empty()
        && let Some(first_line) = body.lines().find(|line| !line.trim().is_empty())
    {
        let units: Vec<u16> = first_line.encode_utf16().collect();
        let mut truncated = String::from_utf16_lossy(&units[..units.len().min(60)]);
        if units.len() > 60 {
            truncated.push_str("...");
        }
        description = truncated;
    }
    let argument_hint = frontmatter
        .get("argument-hint")
        .and_then(Value::as_str)
        .filter(|hint| !hint.is_empty())
        .map(str::to_string);

    let template = PromptTemplate {
        name,
        description,
        argument_hint,
        content: body,
        source_info,
        file_path: file_path.to_path_buf(),
    };
    (Some(template), diagnostics)
}

fn load_templates_from_dir(
    dir: &Path,
    source_info_for: impl Fn(&Path) -> SourceInfo,
) -> LoadPromptTemplatesResult {
    let mut result = LoadPromptTemplatesResult::default();
    if !dir.exists() {
        return result;
    }
    let Ok(read_dir) = fs::read_dir(dir) else {
        return result;
    };
    for entry in read_dir.filter_map(|entry| entry.ok()) {
        let full_path = entry.path();
        let Ok(metadata) = fs::metadata(&full_path) else {
            continue;
        };
        if !metadata.is_file() || !entry.file_name().to_string_lossy().ends_with(".md") {
            continue;
        }
        let (template, diagnostics) =
            load_template_from_file(&full_path, source_info_for(&full_path));
        if let Some(template) = template {
            result.templates.push(template);
        }
        result.diagnostics.extend(diagnostics);
    }
    result
}

fn append_result(target: &mut LoadPromptTemplatesResult, source: LoadPromptTemplatesResult) {
    target.templates.extend(source.templates);
    target.diagnostics.extend(source.diagnostics);
}

/// Load global, project, and explicit prompt templates in that order (no dedup).
pub fn load_prompt_templates(options: &LoadPromptTemplatesOptions) -> LoadPromptTemplatesResult {
    let resolved_cwd = resolve_path_default(&options.cwd);
    let resolved_agent_dir = resolve_path_default(&options.agent_dir);
    let global_prompts_dir = resolved_agent_dir.join("prompts");
    let project_prompts_dir = resolved_cwd.join(CONFIG_DIR_NAME).join("prompts");

    let source_info_for = |path: &Path| -> SourceInfo {
        if is_path_within(&global_prompts_dir, path) {
            create_synthetic_source_info(
                path,
                "local",
                Some(super::skills::SourceScope::User),
                None,
                Some(&global_prompts_dir),
            )
        } else if is_path_within(&project_prompts_dir, path) {
            create_synthetic_source_info(
                path,
                "local",
                Some(super::skills::SourceScope::Project),
                None,
                Some(&project_prompts_dir),
            )
        } else {
            let base_dir = if fs::metadata(path).is_ok_and(|metadata| metadata.is_dir()) {
                path.to_path_buf()
            } else {
                path.parent()
                    .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
            };
            create_synthetic_source_info(path, "local", None, None, Some(&base_dir))
        }
    };

    let mut result = LoadPromptTemplatesResult::default();
    if options.include_defaults {
        append_result(
            &mut result,
            load_templates_from_dir(&global_prompts_dir, source_info_for),
        );
        append_result(
            &mut result,
            load_templates_from_dir(&project_prompts_dir, source_info_for),
        );
    }

    let add_path = |result: &mut LoadPromptTemplatesResult, path: &Path| {
        let Ok(metadata) = fs::metadata(path) else {
            return;
        };
        if metadata.is_dir() {
            append_result(result, load_templates_from_dir(path, source_info_for));
        } else if metadata.is_file() && path.to_string_lossy().ends_with(".md") {
            let (template, diagnostics) = load_template_from_file(path, source_info_for(path));
            if let Some(template) = template {
                result.templates.push(template);
            }
            result.diagnostics.extend(diagnostics);
        }
    };

    for raw_path in &options.prompt_paths {
        // Upstream silently skips explicit paths that do not exist.
        if !raw_path.exists() {
            continue;
        }
        let resolved_path = super::skills::resolve_path(raw_path, &resolved_cwd, true);
        if !resolved_path.exists() {
            continue;
        }
        match fs::metadata(&resolved_path) {
            Ok(_) => add_path(&mut result, &resolved_path),
            Err(error) => result.diagnostics.push(ResourceDiagnostic::warning(
                format!("{error}"),
                Some(&resolved_path),
            )),
        }
    }

    result
}

/// Expand a leading `/command args` string against the first matching template.
pub fn expand_prompt_template(text: &str, templates: &[PromptTemplate]) -> String {
    if !text.starts_with('/') {
        return text.to_string();
    }
    let rest = &text[1..];
    let name_len = rest
        .chars()
        .take_while(|ch| !ch.is_whitespace())
        .map(char::len_utf8)
        .sum::<usize>();
    if name_len == 0 {
        return text.to_string();
    }
    let template_name = &rest[..name_len];
    let args_string = if name_len < rest.len() {
        rest[name_len..].trim_start_matches(char::is_whitespace)
    } else {
        ""
    };

    let Some(template) = templates
        .iter()
        .find(|template| template.name == template_name)
    else {
        return text.to_string();
    };
    substitute_args(&template.content, &parse_command_args(args_string))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn write(path: &Path, content: &str) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| format!("mkdir: {error}"))?;
        }
        fs::write(path, content).map_err(|error| format!("write: {error}"))
    }

    #[test]
    fn simple_placeholders() {
        let input = args(&["a", "b", "c"]);
        assert_eq!(substitute_args("Test: $ARGUMENTS", &input), "Test: a b c");
        assert_eq!(substitute_args("Test: $@", &input), "Test: a b c");
        assert_eq!(
            substitute_args("$ARGUMENTS and $@", &input),
            "a b c and a b c"
        );
    }

    #[test]
    fn no_recursive_substitution() {
        assert_eq!(
            substitute_args("$ARGUMENTS", &args(&["$1", "$ARGUMENTS"])),
            "$1 $ARGUMENTS"
        );
        assert_eq!(substitute_args("$@", &args(&["$100", "$1"])), "$100 $1");
    }

    #[test]
    fn positional_and_out_of_range() {
        let input = args(&["a", "b"]);
        assert_eq!(substitute_args("$1 $2 $3 $4 $5", &input), "a b   ");
        assert_eq!(substitute_args("$0", &input), "");
        assert_eq!(substitute_args("$1.5", &input), "a.5");
        assert_eq!(substitute_args("$1$2", &input), "ab");
        assert_eq!(substitute_args("pre$ARGUMENTS", &input), "prea b");
        assert_eq!(substitute_args("$10 $12", &input), " ");
    }

    #[test]
    fn empty_and_special_arguments() {
        assert_eq!(substitute_args("Test: $ARGUMENTS", &[]), "Test: ");
        assert_eq!(substitute_args("Test: $@", &[]), "Test: ");
        assert_eq!(substitute_args("Test: $1", &[]), "Test: ");
        assert_eq!(substitute_args("$1", &args(&[""])), "");
        assert_eq!(substitute_args("a  c", &args(&["a", "", "c"])), "a  c");
        assert_eq!(
            substitute_args("$A $$ $ $ARGS $arguments $Arguments", &args(&["x"])),
            "$A $$ $ $ARGS $arguments $Arguments"
        );
    }

    #[test]
    fn unicode_and_newlines_are_preserved() {
        assert_eq!(
            substitute_args("$1", &args(&["héllo\nworld\t!"])),
            "héllo\nworld\t!"
        );
    }

    #[test]
    fn positional_defaults() {
        assert_eq!(
            substitute_args("List exactly ${1:-7} next steps", &[]),
            "List exactly 7 next steps"
        );
        assert_eq!(
            substitute_args("List exactly ${1:-7} next steps", &args(&["3"])),
            "List exactly 3 next steps"
        );
        assert_eq!(
            substitute_args("${@:-default}\n${ARGUMENTS:-default}", &[]),
            "default\ndefault"
        );
        assert_eq!(
            substitute_args("Mode: ${1:-brief}", &args(&[""])),
            "Mode: brief"
        );
        assert_eq!(
            substitute_args("${1:-7} ${2:-brief}", &args(&["3"])),
            "3 brief"
        );
        assert_eq!(substitute_args("${3:-fallback}", &args(&["a"])), "fallback");
        assert_eq!(
            substitute_args("${1:-seven steps}", &args(&[])),
            "seven steps"
        );
        assert_eq!(
            substitute_args("$1 ${2:-x} $ARGUMENTS", &args(&["a"])),
            "a x a"
        );
    }

    #[test]
    fn defaults_are_not_substituted_recursively() {
        assert_eq!(substitute_args("${1:-$ARGUMENTS}", &args(&["a", "b"])), "a");
        assert_eq!(
            substitute_args("${3:-$ARGUMENTS}", &args(&["a", "b"])),
            "$ARGUMENTS"
        );
    }

    #[test]
    fn array_slicing() {
        let input = args(&["a", "b", "c", "d"]);
        assert_eq!(substitute_args("${@:2}", &input), "b c d");
        assert_eq!(substitute_args("${@:2:2}", &input), "b c");
        assert_eq!(substitute_args("${@:2:0}", &input), "");
        assert_eq!(substitute_args("${@:99}", &input), "");
        assert_eq!(substitute_args("${@:0}", &input), "a b c d");
        assert_eq!(substitute_args("${@:2:99}", &input), "b c d");
        assert_eq!(substitute_args("${@:2} vs $@", &input), "b c d vs a b c d");
    }

    #[test]
    fn slicing_does_not_substitute_recursively() {
        assert_eq!(
            substitute_args("${@:1}", &args(&["${@:2}", "test"])),
            "${@:2} test"
        );
    }

    #[test]
    fn slices_in_text_and_mixed_forms() {
        let input = args(&["eslint", "file1.ts", "file2.ts", "file3.ts"]);
        assert_eq!(
            substitute_args("Run $1 on ${@:2:2}, then process $@", &input),
            "Run eslint on file1.ts file2.ts, then process eslint file1.ts file2.ts file3.ts"
        );
        assert_eq!(
            substitute_args("prefix${@:2}suffix", &input),
            "prefixfile1.ts file2.ts file3.tssuffix"
        );
    }

    #[test]
    fn parse_simple_arguments() {
        assert_eq!(parse_command_args("a b c"), vec!["a", "b", "c"]);
        assert_eq!(
            parse_command_args("first arg second"),
            vec!["first", "arg", "second"]
        );
        assert_eq!(parse_command_args(""), Vec::<String>::new());
        assert_eq!(parse_command_args("   a\t\tb  "), vec!["a", "b"]);
    }

    #[test]
    fn parse_empty_quoted_arguments() {
        assert_eq!(parse_command_args("\"\" \" \""), vec!["", " "]);
        assert_eq!(parse_command_args("a '' c"), vec!["a", "", "c"]);
        assert_eq!(parse_command_args("a \"\""), vec!["a", ""]);
    }

    #[test]
    fn parse_special_characters_and_newlines() {
        assert_eq!(
            parse_command_args("$100 @user #tag"),
            vec!["$100", "@user", "#tag"]
        );
        assert_eq!(parse_command_args("\"hello\nworld\""), vec!["hello\nworld"]);
        assert_eq!(
            parse_command_args("label-2\n\nHere is some description #2."),
            vec!["label-2", "Here", "is", "some", "description", "#2."]
        );
    }

    #[test]
    fn parse_escaped_quotes_and_literal_backslashes() {
        assert_eq!(
            parse_command_args("\"quoted \\\"text\\\"\""),
            vec!["quoted \"text\""]
        );
        assert_eq!(parse_command_args("'it\\'s here'"), vec!["it's here"]);
        assert_eq!(parse_command_args("C:\\dir\\file"), vec!["C:\\dir\\file"]);
        assert_eq!(
            parse_command_args("\\\\server\\share"),
            vec!["\\\\server\\share"]
        );
        assert_eq!(parse_command_args("\"it's\""), vec!["it's"]);
        assert_eq!(parse_command_args("say \\\"hi"), vec!["say", "\\\"hi"]);
    }

    #[test]
    fn parse_apostrophes_and_unclosed_quotes() {
        assert_eq!(
            parse_command_args("doesn't user's"),
            vec!["doesn't", "user's"]
        );
        assert_eq!(
            parse_command_args("'tis the season"),
            vec!["'tis", "the", "season"]
        );
        assert_eq!(
            parse_command_args("say \"hello world"),
            vec!["say", "\"hello", "world"]
        );
        assert_eq!(parse_command_args("a '"), vec!["a", "'"]);
        assert_eq!(parse_command_args("\"abc\\\""), vec!["\"abc\\\""]);
        assert_eq!(parse_command_args("\"it's\" 'x"), vec!["it's", "'x"]);
    }

    fn template(name: &str, content: &str) -> PromptTemplate {
        let path = PathBuf::from(format!("/tmp/{name}.md"));
        PromptTemplate {
            name: name.to_string(),
            description: String::new(),
            argument_hint: None,
            content: content.to_string(),
            source_info: create_synthetic_source_info(&path, "test", None, None, None),
            file_path: path,
        }
    }

    #[test]
    fn expand_only_slash_prefixed_text() {
        let templates = vec![template("review", "Review $2. Focus: ${1:-correctness}.")];
        assert_eq!(
            expand_prompt_template("/review \"\" src/index.ts", &templates),
            "Review src/index.ts. Focus: correctness."
        );
        assert_eq!(expand_prompt_template("review x", &templates), "review x");
        assert_eq!(
            expand_prompt_template("/missing x", &templates),
            "/missing x"
        );
    }

    #[test]
    fn expand_multiline_arguments() {
        let templates = vec![template("arg-test", "- arg1: $1\n- rest: ${@:2}")];
        assert_eq!(
            expand_prompt_template(
                "/arg-test label-2\n\nHere is some description #2.",
                &templates
            ),
            "- arg1: label-2\n- rest: Here is some description #2."
        );
        assert_eq!(
            expand_prompt_template("/arg-test\nlabel-2", &templates),
            "- arg1: label-2\n- rest: "
        );
    }

    #[test]
    fn expand_quoted_arguments() {
        let templates = vec![template(
            "component",
            "Create component $1 with features: $ARGUMENTS",
        )];
        assert_eq!(
            expand_prompt_template(
                "\"Button\" \"onClick handler\" \"disabled support\"",
                &templates
            ),
            "\"Button\" \"onClick handler\" \"disabled support\""
        );
        assert_eq!(
            expand_prompt_template(
                "/component Button \"onClick handler\" \"disabled support\"",
                &templates
            ),
            "Create component Button with features: Button onClick handler disabled support"
        );
    }

    #[test]
    fn load_templates_with_frontmatter_and_fallback_description() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| format!("tempdir: {error}"))?;
        let prompts = dir.path().join("prompts");
        write(
            &prompts.join("review.md"),
            "---\ndescription: Review code\nargument-hint: \"<PR-URL>\"\n---\nReview $1\n",
        )?;
        write(
            &prompts.join("plain.md"),
            "First line of the body\nMore text\n",
        )?;
        let result = load_prompt_templates(&LoadPromptTemplatesOptions {
            cwd: dir.path().to_path_buf(),
            agent_dir: dir.path().to_path_buf(),
            include_defaults: true,
            ..Default::default()
        });
        assert_eq!(result.templates.len(), 2);
        let review = result
            .templates
            .iter()
            .find(|item| item.name == "review")
            .ok_or("no review")?;
        assert_eq!(review.description, "Review code");
        assert_eq!(review.argument_hint.as_deref(), Some("<PR-URL>"));
        let plain = result
            .templates
            .iter()
            .find(|item| item.name == "plain")
            .ok_or("no plain")?;
        assert_eq!(plain.description, "First line of the body");
        assert!(result.diagnostics.is_empty());
        Ok(())
    }

    #[test]
    fn load_templates_invalid_yaml_diagnostic() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| format!("tempdir: {error}"))?;
        let prompts = dir.path().join("prompts");
        write(&prompts.join("valid.md"), "Valid body\n")?;
        write(
            &prompts.join("invalid.md"),
            "---\ndescription: Broken: unquoted colon\n---\nBody\n",
        )?;
        let result = load_prompt_templates(&LoadPromptTemplatesOptions {
            cwd: dir.path().to_path_buf(),
            agent_dir: dir.path().to_path_buf(),
            include_defaults: true,
            ..Default::default()
        });
        assert_eq!(result.templates.len(), 1);
        assert_eq!(result.templates[0].name, "valid");
        assert_eq!(result.diagnostics.len(), 1);
        assert!(
            result.diagnostics[0].message.contains("line 1, column 14"),
            "{}",
            result.diagnostics[0].message
        );
        Ok(())
    }

    #[test]
    fn explicit_paths_are_not_deduplicated() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| format!("tempdir: {error}"))?;
        let first = dir.path().join("a.md");
        write(&first, "Body\n")?;
        let result = load_prompt_templates(&LoadPromptTemplatesOptions {
            cwd: dir.path().to_path_buf(),
            agent_dir: dir.path().join("agent"),
            prompt_paths: vec![first.clone(), first],
            include_defaults: false,
        });
        assert_eq!(result.templates.len(), 2);
        Ok(())
    }
}
