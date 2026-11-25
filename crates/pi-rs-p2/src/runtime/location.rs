pub use super::session_worker::Location;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocationInput {
    pub env: Option<String>,
    pub path: String,
}

pub fn same_location(a: &Location, b: &Location) -> bool {
    a == b
}
pub fn location_text(loc: &Location) -> String {
    match loc.env.as_deref().filter(|s| !s.is_empty()) {
        Some(env) => format!("{env}:{}", loc.cwd),
        None => loc.cwd.clone(),
    }
}
pub fn short_path(path: &str) -> String {
    for prefix in ["/Users/", "/home/"] {
        if let Some(rest) = path.strip_prefix(prefix) {
            let (user, tail) = rest
                .split_once('/')
                .map_or((rest, ""), |(u, _)| (u, &rest[u.len()..]));
            if !user.is_empty() {
                return format!("~{tail}");
            }
        }
    }
    for prefix in ["/root", "/data/data/com.termux/files/home"] {
        if let Some(rest) = path.strip_prefix(prefix)
            && (rest.is_empty() || rest.starts_with('/'))
        {
            return format!("~{rest}");
        }
    }
    path.into()
}
fn drive(path: &str) -> bool {
    matches!(path.as_bytes(),[a,b':',b'/'|b'\\',..] if a.is_ascii_alphabetic())
}
pub fn is_rooted(path: &str) -> bool {
    path == "~"
        || path.starts_with("~/")
        || path.starts_with('/')
        || drive(path)
        || path.starts_with("\\\\")
}
pub fn join_path(base: &str, path: &str) -> String {
    let windows = drive(base) || base.contains('\\');
    let sep = if windows { "\\" } else { "/" };
    let drive = if windows
        && base.as_bytes().get(1) == Some(&b':')
        && base.as_bytes()[0].is_ascii_alphabetic()
    {
        &base[..2]
    } else {
        ""
    };
    let base = if windows {
        base.chars().skip(drive.len() + 1).collect::<String>()
    } else {
        base.into()
    };
    let mut parts: Vec<_> = base.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    for part in path.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    format!("{drive}{sep}{}", parts.join(sep))
}
pub const LOCAL: &str = "local";
pub fn parse_location(
    text: &str,
    envs: &[String],
    fallback_env: Option<&str>,
    cwd: Option<&str>,
) -> Result<LocationInput, String> {
    let mut path = text.trim();
    let mut env = fallback_env;
    if let Some((name, rest)) = path.split_once(':') {
        let valid = !name.is_empty()
            && name.len() <= 32
            && name.bytes().enumerate().all(|(i, c)| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || (i > 0 && matches!(c, b'_' | b'-'))
            });
        let known = envs.iter().any(|e| e == name);
        if valid && (known || name.len() > 1) {
            env = if known {
                Some(name)
            } else if name == LOCAL {
                None
            } else {
                return Err(format!("Unknown environment \"{name}\" (/envs lists them)"));
            };
            path = rest.trim();
        }
    }
    let base = cwd.filter(|s| env == fallback_env && !s.is_empty());
    let path = if path.is_empty() {
        base.unwrap_or("~").into()
    } else if is_rooted(path) {
        path.into()
    } else {
        base.map_or_else(|| format!("~/{path}"), |b| join_path(b, path))
    };
    Ok(LocationInput {
        env: env.map(str::to_owned),
        path,
    })
}
