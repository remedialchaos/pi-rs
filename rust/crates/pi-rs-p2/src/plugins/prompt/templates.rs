//! Prompt templates, frontmatter parsing, argument substitution, and skill expansion.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::contract::TemplateSource;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptTemplate {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
    pub content: String,
    pub source: TemplateSource,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillFile {
    pub name: String,
    pub description: String,
    pub path: String,
    pub base_dir: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpandedPrompt {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill: Option<ExpandedSkill>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpandedSkill {
    pub name: String,
}

fn normalize_newlines(value: &str) -> String {
    value.replace("\r\n", "\n").replace('\r', "\n")
}

fn unquote_yaml(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.len() >= 2
        && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
    {
        return trimmed[1..trimmed.len() - 1].to_string();
    }
    trimmed.to_string()
}

/// Parses YAML frontmatter between leading `---` lines; returns `(frontmatter, body)`.
pub fn parse_frontmatter(content: &str) -> (IndexMap<String, Value>, String) {
    let stripped = content.strip_prefix('\u{FEFF}').unwrap_or(content);
    let normalized = normalize_newlines(stripped);
    if !normalized.starts_with("---") {
        return (IndexMap::new(), normalized);
    }
    let Some(end_rel) = normalized[3..].find("\n---") else {
        return (IndexMap::new(), normalized);
    };
    let yaml_block = &normalized[4.min(normalized.len())..3 + end_rel];
    let body = normalized[3 + end_rel + 4..].trim().to_string();

    let mut map = IndexMap::new();
    for line in yaml_block.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, val)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim().to_string();
        let val_str = val.trim();
        let parsed = if val_str == "true" {
            Value::Bool(true)
        } else if val_str == "false" {
            Value::Bool(false)
        } else {
            Value::String(unquote_yaml(val_str))
        };
        map.insert(key, parsed);
    }
    (map, body)
}

/// Parses a `.md` template file into `PromptTemplate`.
pub fn parse_template(
    file_name: &str,
    content: &str,
    source: TemplateSource,
    path: &str,
) -> PromptTemplate {
    let (frontmatter, body) = parse_frontmatter(content);
    let name = file_name
        .strip_suffix(".md")
        .unwrap_or(file_name)
        .to_string();
    let mut description = frontmatter
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if description.is_empty()
        && let Some(first_line) = body.lines().find(|l| !l.trim().is_empty())
    {
        let count = first_line.chars().count();
        description = if count > 60 {
            let prefix: String = first_line.chars().take(60).collect();
            format!("{prefix}...")
        } else {
            first_line.to_string()
        };
    }
    let argument_hint = frontmatter
        .get("argument-hint")
        .and_then(Value::as_str)
        .map(str::to_string);
    PromptTemplate {
        name,
        description,
        argument_hint,
        content: body,
        source,
        path: path.to_string(),
    }
}

/// Bash-style argument splitting with quotes (`parseCommandArgs`).
pub fn parse_command_args(args_string: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quote: Option<char> = None;
    for ch in args_string.chars() {
        if let Some(q) = in_quote {
            if ch == q {
                in_quote = None;
            } else {
                current.push(ch);
            }
        } else if ch == '"' || ch == '\'' {
            in_quote = Some(ch);
        } else if ch.is_whitespace() {
            if !current.is_empty() {
                args.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

/// Substitutes `$1`..., `$@`, `$ARGUMENTS`, `${N:-default}`, `${@:-default}`, `${@:N}`, `${@:N:L}`.
pub fn substitute_args(content: &str, args: &[String]) -> String {
    let all_args = args.join(" ");
    let mut out = String::with_capacity(content.len());
    let bytes = content.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] != b'$' {
            let ch = content[i..].chars().next().unwrap_or('\0');
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let rest = &content[i + 1..];
        if let Some(inner) = rest.strip_prefix('{')
            && let Some(close) = inner.find('}')
        {
            let expr = &inner[..close];
            if let Some((target, default_val)) = expr.split_once(":-") {
                let resolved = if target == "@" || target == "ARGUMENTS" {
                    (!all_args.is_empty()).then_some(all_args.as_str())
                } else if let Ok(idx) = target.parse::<usize>()
                    && idx >= 1
                {
                    args.get(idx - 1)
                        .filter(|s| !s.is_empty())
                        .map(String::as_str)
                } else {
                    None
                };
                out.push_str(resolved.unwrap_or(default_val));
                i += 2 + close + 1;
                continue;
            }
            if let Some(slice_expr) = expr.strip_prefix("@:") {
                let mut parts = slice_expr.split(':');
                if let Some(Ok(start_1)) = parts.next().map(str::parse::<usize>) {
                    let start = start_1.saturating_sub(1);
                    let len_opt = parts.next().and_then(|s| s.parse::<usize>().ok());
                    if parts.next().is_none() {
                        let joined = if start >= args.len() {
                            String::new()
                        } else if let Some(len) = len_opt {
                            let end = (start + len).min(args.len());
                            args[start..end].join(" ")
                        } else {
                            args[start..].join(" ")
                        };
                        out.push_str(&joined);
                        i += 2 + close + 1;
                        continue;
                    }
                }
            }
        }
        if let Some(after) = rest.strip_prefix("ARGUMENTS") {
            let _ = after;
            out.push_str(&all_args);
            i += 1 + "ARGUMENTS".len();
            continue;
        }
        if rest.starts_with('@') {
            out.push_str(&all_args);
            i += 2;
            continue;
        }
        let digit_len = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
        if digit_len > 0
            && let Ok(idx) = rest[..digit_len].parse::<usize>()
        {
            if idx >= 1
                && let Some(arg) = args.get(idx - 1)
            {
                out.push_str(arg);
            }
            i += 1 + digit_len;
            continue;
        }
        out.push('$');
        i += 1;
    }
    out
}

/// The skill block: the skill's body with its name and location, then the user's arguments.
pub fn skill_block(skill: &SkillFile, file_content: &str, args: &str) -> String {
    let (_, body) = parse_frontmatter(file_content);
    let body = body.trim();
    let block = format!(
        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
        skill.name, skill.path, skill.base_dir, body
    );
    if args.is_empty() {
        block
    } else {
        format!("{block}\n\n{args}")
    }
}

/// Expands `/skill:name args` or `/template args`; unknown names or unreadable skills stay unchanged.
pub async fn expand_prompt<F, Fut>(
    text: &str,
    templates: &[PromptTemplate],
    skills: &[SkillFile],
    read_skill: F,
) -> ExpandedPrompt
where
    F: FnOnce(&SkillFile) -> Fut,
    Fut: std::future::Future<Output = Option<String>>,
{
    if let Some(rest) = text.strip_prefix("/skill:") {
        let (name, args) = match rest.find(' ') {
            Some(idx) => (&rest[..idx], rest[idx + 1..].trim()),
            None => (rest, ""),
        };
        let Some(skill) = skills.iter().find(|s| s.name == name) else {
            return ExpandedPrompt {
                text: text.to_string(),
                skill: None,
            };
        };
        return match read_skill(skill).await {
            Some(content) => ExpandedPrompt {
                text: skill_block(skill, &content, args),
                skill: Some(ExpandedSkill {
                    name: skill.name.clone(),
                }),
            },
            None => ExpandedPrompt {
                text: text.to_string(),
                skill: None,
            },
        };
    }

    if !text.starts_with('/') {
        return ExpandedPrompt {
            text: text.to_string(),
            skill: None,
        };
    }
    let rest = &text[1..];
    let mut split_at = rest.len();
    for (idx, ch) in rest.char_indices() {
        if ch.is_whitespace() {
            split_at = idx;
            break;
        }
    }
    let name = &rest[..split_at];
    if name.is_empty() {
        return ExpandedPrompt {
            text: text.to_string(),
            skill: None,
        };
    }
    let args_str = rest[split_at..].trim_start();
    let Some(template) = templates.iter().find(|t| t.name == name) else {
        return ExpandedPrompt {
            text: text.to_string(),
            skill: None,
        };
    };
    let parsed_args = parse_command_args(args_str);
    ExpandedPrompt {
        text: substitute_args(&template.content, &parsed_args),
        skill: None,
    }
}
