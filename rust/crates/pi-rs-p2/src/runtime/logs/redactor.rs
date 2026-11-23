use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use regex::Regex;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::LazyLock,
};
use url::{Url, form_urlencoded};

const MASK: &str = "[redacted]";
const URI: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');
fn pattern(s: &str) -> Regex {
    // INFALLIBLE: all patterns are fixed and exercised by the log redaction tests.
    Regex::new(s).expect("valid static log pattern")
}
static FIELD: LazyLock<Regex> = LazyLock::new(|| {
    pattern(
        r"(?i)api[-_]?key|access[-_]?token|refresh[-_]?token|token|secret|password|authorization|cookie|credential|private[-_]?key|^key$|^access$|^refresh$",
    )
});
static RULES: LazyLock<Vec<(Regex, &str)>> = LazyLock::new(|| {
    vec![
        (
            pattern(r"(?i)\b(authorization|proxy-authorization|cookie|set-cookie)\s*:\s*[^\r\n]*"),
            "${1}: [redacted]",
        ),
        (
            pattern(r"(?i)\b(Bearer|Basic)\s+[A-Za-z0-9+/=_~.%-]+"),
            "${1} [redacted]",
        ),
        (
            pattern(r"(?i)(https?://)[^\s/@]+:[^\s/@]+@"),
            "${1}[redacted]@",
        ),
        (pattern(r"(?-u:\b)sk-[A-Za-z0-9_-]+(?-u:\b)"), MASK),
        (
            pattern(
                r#"(?i)(["']?(?:api[-_]?key|access[-_]?token|refresh[-_]?token|token|secret|password|cookie|authorization|proxy[-_]?secret|key|access|refresh)["']?\s*[:=]\s*)(?:"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|[^\s,;&}]+)"#,
            ),
            "${1}[redacted]",
        ),
    ]
});
#[derive(PartialEq)]
struct Fingerprint {
    modified: std::time::SystemTime,
    size: u64,
    #[cfg(unix)]
    identity: (u64, i64, i64),
}
impl Fingerprint {
    fn from(meta: &std::fs::Metadata) -> std::io::Result<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            modified: meta.modified()?,
            size: meta.len(),
            #[cfg(unix)]
            identity: (meta.ino(), meta.ctime(), meta.ctime_nsec()),
        })
    }
}
#[derive(Default)]
pub struct LogRedactor {
    secrets: HashSet<String>,
    directories: HashSet<PathBuf>,
    fingerprints: HashMap<PathBuf, Fingerprint>,
}
impl LogRedactor {
    pub fn add_secret(&mut self, s: &str) {
        if s.is_empty() || s == MASK {
            return;
        }
        for v in [s, s.trim()].into_iter().chain(s.lines()) {
            if !v.is_empty() && v != MASK {
                self.secrets.insert(v.into());
            }
        }
        if let Ok(u) = Url::parse(s) {
            for v in [Some(u.username()), u.password()]
                .into_iter()
                .flatten()
                .filter(|v| !v.is_empty())
            {
                self.secrets
                    .insert(percent_decode_str(v).decode_utf8_lossy().into_owned());
            }
            for (k, v) in u.query_pairs().chain(form_urlencoded::parse(
                u.fragment().unwrap_or_default().as_bytes(),
            )) {
                if !v.is_empty()
                    && matches!(
                        k.to_ascii_lowercase().as_str(),
                        "code"
                            | "state"
                            | "token"
                            | "access_token"
                            | "refresh_token"
                            | "id_token"
                            | "api_key"
                    )
                {
                    self.secrets.insert(v.into_owned());
                }
            }
        }
    }
    pub fn add_directory(&mut self, path: PathBuf) {
        self.directories.insert(path);
    }
    fn collect(&mut self, v: &Value, sensitive: bool) {
        match v {
            Value::String(s) if sensitive => self.add_secret(s),
            Value::Array(a) => a.iter().for_each(|v| self.collect(v, sensitive)),
            Value::Object(o) => o
                .iter()
                .for_each(|(k, v)| self.collect(v, sensitive || FIELD.is_match(k))),
            _ => {}
        }
    }
    pub fn redact(&mut self, s: &str) -> String {
        for (k, v) in std::env::vars_os() {
            if FIELD.is_match(&k.to_string_lossy()) {
                self.add_secret(&v.to_string_lossy());
            }
        }
        for dir in self.directories.clone() {
            for name in [
                "auth.json",
                "radius.json",
                "models.json",
                "settings.json",
                "envs.json",
                "sandbox-secrets.json",
                "discord.json",
                "owner-key",
                "owner-token.txt",
            ] {
                let path = dir.join(name);
                let fingerprint = std::fs::metadata(&path)
                    .and_then(|m| Fingerprint::from(&m))
                    .ok();
                if fingerprint
                    .as_ref()
                    .is_some_and(|f| self.fingerprints.get(&path) == Some(f))
                {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                if name.ends_with(".json") {
                    if let Ok(v) =
                        serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}'))
                    {
                        if name == "sandbox-secrets.json" {
                            for group in v.as_object().into_iter().flat_map(|o| o.values()) {
                                for sec in group.as_object().into_iter().flat_map(|o| o.values()) {
                                    if let Some(s) = sec["value"].as_str() {
                                        self.add_secret(s)
                                    }
                                }
                            }
                        }
                        self.collect(&v, false)
                    } else {
                        continue;
                    }
                } else {
                    self.add_secret(text.trim())
                }
                if let Some(f) = fingerprint {
                    self.fingerprints.insert(path, f);
                }
            }
        }
        let mut secrets: Vec<_> = self.secrets.iter().collect();
        secrets.sort_by_cached_key(|s| std::cmp::Reverse(s.encode_utf16().count()));
        let mut out = s.to_owned();
        for v in secrets {
            out = out.replace(v, MASK);
            let enc = utf8_percent_encode(v, URI).to_string();
            if enc != *v {
                out = out.replace(&enc, MASK)
            }
        }
        for (re, replacement) in RULES.iter() {
            out = re.replace_all(&out, *replacement).into_owned()
        }
        out
    }
}
