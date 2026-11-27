use crate::{Result, write_json};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};
use url::Url;

pub type Environment = BTreeMap<String, String>;

pub fn environment() -> Environment {
    std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .collect()
}

pub trait Preferences {
    fn get(&self, key: &str) -> Option<String>;
    fn set(&mut self, key: &str, value: Option<&str>) -> Result<()>;
}

pub struct JsonPreferences {
    path: PathBuf,
    values: BTreeMap<String, String>,
}
impl JsonPreferences {
    pub fn open(path: PathBuf) -> Result<Self> {
        let values = match fs::File::open(&path) {
            Ok(file) => serde_json::from_reader(file)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self { path, values })
    }
}
impl Preferences for JsonPreferences {
    fn get(&self, key: &str) -> Option<String> {
        self.values.get(key).cloned()
    }
    fn set(&mut self, key: &str, value: Option<&str>) -> Result<()> {
        let old = match value {
            Some(v) => self.values.insert(key.into(), v.into()),
            None => self.values.remove(key),
        };
        if let Err(error) = write_json(&self.path, &self.values) {
            if let Some(old) = old {
                self.values.insert(key.into(), old);
            } else {
                self.values.remove(key);
            }
            return Err(error);
        }
        Ok(())
    }
}
impl Preferences for BTreeMap<String, String> {
    fn get(&self, key: &str) -> Option<String> {
        BTreeMap::get(self, key).cloned()
    }
    fn set(&mut self, key: &str, value: Option<&str>) -> Result<()> {
        if let Some(v) = value {
            self.insert(key.into(), v.into());
        } else {
            self.remove(key);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Own,
    Attached,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Start,
    Dev,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerConfig {
    pub source: Source,
    /// The native `p2` binary the app runs; `None`: `p2` on the login shell's PATH.
    pub binary: Option<PathBuf>,
    pub port: u16,
    pub data_dir: PathBuf,
    pub mode: Mode,
    pub fake: bool,
    pub runs_bundled: bool,
}

#[derive(Clone, Debug)]
pub struct Bundled {
    /// The native `p2` binary the app carries in its resources.
    pub binary: PathBuf,
}
impl Bundled {
    pub fn find(resources: &Path) -> Option<Self> {
        let binary = resources.join("bin/p2");
        crate::login_environment::executable(&binary).then_some(Self { binary })
    }
}

pub fn expand(home: &Path, value: &str) -> PathBuf {
    if value == "~" {
        return home.to_owned();
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return home.join(rest);
    }
    PathBuf::from(value)
}

/// The path with the home directory written as `~`, like macOS's `abbreviatingWithTildeInPath`.
pub fn abbreviate(home: &Path, path: &Path) -> String {
    let Ok(rest) = path.strip_prefix(home) else {
        return path.to_string_lossy().into_owned();
    };
    let rest = rest.to_string_lossy();
    if rest.is_empty() {
        return "~".to_owned();
    }
    format!("~/{rest}")
}

pub fn home(env: &Environment) -> PathBuf {
    env.get("HOME")
        .or_else(|| env.get("USERPROFILE"))
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
}

#[derive(Debug, Clone)]
pub struct Launch {
    pub selected: ServerConfig,
    pub own: ServerConfig,
    pub home: PathBuf,
    pub brains: Option<PathBuf>,
    pub sign_in_headless: bool,
    pub diag_dir: Option<PathBuf>,
    pub diag_bridge: Option<String>,
    pub diag_open: Vec<String>,
    pub diag_path: String,
    pub gateway: Url,
}

impl Launch {
    pub fn load(
        env: &Environment,
        prefs: &dyn Preferences,
        bundled: Option<&Bundled>,
    ) -> Result<Self> {
        let home = home(env);
        let default_data = home.join(".pi/p2");
        let value = |key, variable| env.get(variable).cloned().or_else(|| prefs.get(key));
        let path = |s: String| expand(&home, &s);
        let own_data = if env.contains_key("P2MAC_ATTACH") {
            prefs.get("dataDir")
        } else {
            value("dataDir", "P2MAC_DATA_DIR")
        };
        let binary = value("binary", "P2MAC_BINARY")
            .map(path)
            .filter(|binary| crate::login_environment::executable(binary))
            .or_else(|| bundled.map(|b| b.binary.clone()));
        let own = ServerConfig {
            source: Source::Own,
            runs_bundled: bundled.is_some_and(|b| binary.as_ref() == Some(&b.binary)),
            binary,
            port: value("port", "P2MAC_PORT")
                .and_then(|p| p.parse().ok())
                .unwrap_or(8788),
            data_dir: own_data.map(path).unwrap_or_else(|| default_data.clone()),
            mode: if value("mode", "P2MAC_MODE").as_deref() == Some("dev") {
                Mode::Dev
            } else {
                Mode::Start
            },
            fake: value("fake", "P2MAC_FAKE").as_deref() == Some("1"),
        };
        let selected = if let Some(port) = env.get("P2MAC_ATTACH").and_then(|p| p.parse().ok()) {
            own.attached(
                port,
                env.get("P2MAC_DATA_DIR")
                    .cloned()
                    .map(path)
                    .unwrap_or(default_data),
            )
        } else if prefs.get("source").as_deref() == Some("attached")
            && let Some(port) = prefs.get("attachedPort").and_then(|p| p.parse().ok())
        {
            own.attached(
                port,
                prefs
                    .get("attachedDataDir")
                    .map(path)
                    .unwrap_or(default_data),
            )
        } else {
            own.clone()
        };
        Ok(Self {
            selected,
            own,
            brains: env.get("P2MAC_BRAINS").cloned().map(path),
            diag_dir: env.get("P2MAC_DIAG_DIR").map(PathBuf::from),
            diag_bridge: env.get("P2MAC_DIAG_BRIDGE").cloned(),
            diag_open: env
                .get("P2MAC_DIAG_OPEN")
                .into_iter()
                .flat_map(|s| s.split(','))
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
            diag_path: env
                .get("P2MAC_DIAG_PATH")
                .cloned()
                .unwrap_or_else(|| "/".into()),
            sign_in_headless: env
                .get("P2MAC_SIGN_IN")
                .is_some_and(|value| value == "headless"),
            gateway: Url::parse(
                env.get("PI_RADIUS_GATEWAY")
                    .map_or("https://radius.pi.dev", String::as_str),
            )?,
            home,
        })
    }
}

impl ServerConfig {
    pub fn origin(&self) -> Url {
        // INFALLIBLE: constant HTTP loopback URL with a u16 port.
        Url::parse(&format!("http://127.0.0.1:{}/", self.port)).expect("valid loopback origin")
    }
    pub fn attached(&self, port: u16, data_dir: PathBuf) -> Self {
        Self {
            source: Source::Attached,
            port,
            data_dir,
            ..self.clone()
        }
    }
    pub fn save_selection(&self, preferences: &mut dyn Preferences) -> Result<()> {
        if self.source == Source::Attached {
            preferences.set("attachedPort", Some(&self.port.to_string()))?;
            preferences.set("attachedDataDir", self.data_dir.to_str())?;
            preferences.set("source", Some("attached"))
        } else {
            preferences.set("source", Some("own"))
        }
    }
}
