//! `projects.json` parsing & validation matching `apps/p2/plugins/projects/config.ts`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use pi_rs_chord::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectPlace {
    pub env: Option<String>,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub name: String,
    pub places: Vec<ProjectPlace>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchivedProject {
    pub name: String,
    pub places: Vec<ProjectPlace>,
    pub archived_at: u64,
}

fn is_env_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > 32 {
        return false;
    }
    let first = bytes[0];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
}

fn has_control(value: &str) -> bool {
    value
        .chars()
        .any(|c| (c as u32) < 0x20 || (c as u32) == 0x7f)
}

/// Rooted path: POSIX `/...`, Windows drive `C:\...` / `C:/...`, UNC `\\...`, or `~` / `~/...`.
pub fn is_rooted(path: &str) -> bool {
    if path.starts_with('/')
        || path.starts_with("\\\\")
        || path == "~"
        || path.starts_with("~/")
        || path.starts_with("~\\")
    {
        return true;
    }
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

fn strip_bom(content: &str) -> &str {
    content.strip_prefix('\u{FEFF}').unwrap_or(content)
}

/// Strip `//` and `/* ... */` comments outside JSON strings (matching `stripJsonComments`).
fn strip_json_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
        } else if c == '/' && chars.peek() == Some(&'/') {
            chars.next();
            for next in chars.by_ref() {
                if next == '\n' {
                    out.push('\n');
                    break;
                }
            }
        } else if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            while let Some(next) = chars.next() {
                if next == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Parses `projects.json` text, throwing with the first problem naming the project.
pub fn parse_projects(content: &str) -> Result<Vec<Project>> {
    let stripped = strip_json_comments(strip_bom(content));
    let value: Value = serde_json::from_str(&stripped)
        .map_err(|_| Error::Invalid("Invalid projects.json syntax".into()))?;
    let Some(map) = value.as_object() else {
        return Err(Error::Invalid(
            "projects.json must be an object of projects".into(),
        ));
    };
    let mut out = Vec::with_capacity(map.len());
    for (name, places) in map {
        out.push(Project {
            name: name.clone(),
            places: parse_places(name, places)?,
        });
    }
    Ok(out)
}

/// Validates a project's name and places: absolute or `~` paths, at most one per environment.
pub fn parse_places(name: &str, places: &Value) -> Result<Vec<ProjectPlace>> {
    if name.trim().is_empty() || name.chars().count() > 64 || has_control(name) {
        return Err(Error::Invalid(format!("Invalid project name: {name}")));
    }
    let Some(list) = places.as_array() else {
        return Err(Error::Invalid(format!(
            "Project {name}: a list of places ({{ \"env\"?, \"path\" }}) is required"
        )));
    };
    if list.is_empty() {
        return Err(Error::Invalid(format!(
            "Project {name}: a list of places ({{ \"env\"?, \"path\" }}) is required"
        )));
    }
    let mut seen: HashSet<Option<String>> = HashSet::new();
    let mut parsed = Vec::with_capacity(list.len());
    for item in list {
        let Some(obj) = item.as_object() else {
            return Err(Error::Invalid(format!(
                "Project {name}: a place is {{ \"env\"?: string, \"path\": string }}"
            )));
        };
        for key in obj.keys() {
            if key != "env" && key != "path" {
                return Err(Error::Invalid(format!(
                    "Project {name}: a place is {{ \"env\"?: string, \"path\": string }}"
                )));
            }
        }
        let env = match obj.get("env") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if !s.is_empty() && s.len() <= 32 => Some(s.clone()),
            _ => {
                return Err(Error::Invalid(format!(
                    "Project {name}: a place is {{ \"env\"?: string, \"path\": string }}"
                )));
            }
        };
        let Some(path) = obj
            .get("path")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && s.len() <= 4096)
        else {
            return Err(Error::Invalid(format!(
                "Project {name}: a place is {{ \"env\"?: string, \"path\": string }}"
            )));
        };
        if let Some(ref env_name) = env
            && !is_env_name(env_name)
        {
            return Err(Error::Invalid(format!(
                "Project {name}: invalid environment name {env_name}"
            )));
        }
        if !is_rooted(path) || has_control(path) {
            return Err(Error::Invalid(format!(
                "Project {name}: the path must be absolute or start with ~ ({path})"
            )));
        }
        if !seen.insert(env.clone()) {
            return Err(Error::Invalid(format!(
                "Project {name}: two places on {}",
                env.as_deref().unwrap_or("this device")
            )));
        }
        parsed.push(ProjectPlace {
            env,
            path: path.to_owned(),
        });
    }
    Ok(parsed)
}

pub fn projects_path(directory: &Path) -> PathBuf {
    directory.join("projects.json")
}

pub fn load_projects(directory: &Path) -> Result<Vec<Project>> {
    match std::fs::read_to_string(projects_path(directory)) {
        Ok(text) => parse_projects(&text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(Error::Invalid(err.to_string())),
    }
}
