//! A Pi is its data directory. Which one a command means: the default Pi (`~/.pi/p3`), a named one
//! (`~/.pi/p3/pis/<name>`) or any directory; the port each keeps (`pi.json`, from its first start);
//! and the named Pis.
//!
//! The `*_in` functions take the home directory they work under, so tests never move the process's
//! `HOME`; the public functions read it once per call.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::Value;

use crate::files::{FileError, write_private};

/// Which Pi a command line names: `--pi <name>`, `--dir <path>`, or neither (the default Pi).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PiChoice<'a> {
    pub pi: Option<&'a str>,
    pub dir: Option<&'a str>,
}

/// The default Pi's port (p2 keeps 8788). Every other Pi takes a free one at its first start and
/// keeps it.
pub const DEFAULT_PORT: u16 = 8789;

/// What is wrong with a Pi's `pi.json`.
#[derive(Debug, thiserror::Error)]
pub enum PiError {
    #[error("{path}: \"port\" must be a port number")]
    InvalidPort { path: PathBuf },
    #[error(transparent)]
    File(#[from] FileError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// Whether `name` can name a Pi: lower case letters, digits, `-` and `_`, at most 32.
pub fn is_pi_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    if name.len() > 32 {
        return false;
    }
    bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// The default Pi's data directory under `home`: `~/.pi/p3`.
pub fn default_data_dir_in(home: &Path) -> PathBuf {
    home.join(".pi").join("p3")
}

/// Where the named Pis live under `home`.
pub fn pis_root_in(home: &Path) -> PathBuf {
    default_data_dir_in(home).join("pis")
}

/// The data directory of the Pi `choice` names under `home`.
pub fn data_dir_of_in(home: &Path, choice: PiChoice<'_>) -> PathBuf {
    if let Some(dir) = choice.dir {
        return PathBuf::from(dir);
    }
    match choice.pi {
        None => default_data_dir_in(home),
        Some(name) => pis_root_in(home).join(name),
    }
}

/// The data directory of the Pi `choice` names.
pub fn data_dir_of(choice: PiChoice<'_>) -> Result<PathBuf, FileError> {
    Ok(data_dir_of_in(&crate::files::home_dir()?, choice))
}

/// The port the Pi on `data_dir` keeps: its `pi.json`'s, else 8789 for the default Pi and 0 (a free
/// one) for others.
pub fn kept_port_in(home: &Path, data_dir: &Path) -> Result<(u16, bool), PiError> {
    if let Some(port) = read_config(data_dir)?.and_then(|config| config.port) {
        return Ok((port, true));
    }
    let port = if default_data_dir_in(home) == data_dir {
        DEFAULT_PORT
    } else {
        0
    };
    Ok((port, false))
}

/// The port the Pi on `data_dir` keeps.
pub fn kept_port(data_dir: &Path) -> Result<(u16, bool), PiError> {
    kept_port_in(&crate::files::home_dir()?, data_dir)
}

/// Keeps `port` as the Pi's port (`pi.json`), written with the tab indentation upstream writes.
pub fn keep_port(data_dir: &Path, port: u16) -> Result<(), PiError> {
    let mut buffer = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"\t");
    let mut serializer = serde_json::Serializer::with_formatter(&mut buffer, formatter);
    serde::Serialize::serialize(&serde_json::json!({ "port": port }), &mut serializer).map_err(
        |source| PiError::Json {
            path: config_file(data_dir),
            source,
        },
    )?;
    let text = String::from_utf8(buffer).map_err(|error| PiError::Json {
        path: config_file(data_dir),
        source: serde_json::Error::io(std::io::Error::other(error)),
    })?;
    write_private(&config_file(data_dir), &text)?;
    Ok(())
}

/// The Pi's config, or `None` before its first start.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PiConfig {
    pub port: Option<u16>,
}

/// The Pi's config, or `None` before its first start.
pub fn read_config(data_dir: &Path) -> Result<Option<PiConfig>, PiError> {
    let path = config_file(data_dir);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    // A trust boundary: a file people edit.
    let value: Value =
        serde_json::from_str(&text).map_err(|source| PiError::Json { path, source })?;
    let port = match value.get("port") {
        None => None,
        Some(port) => {
            let invalid = || PiError::InvalidPort {
                path: config_file(data_dir),
            };
            let port = port
                .as_u64()
                .filter(|port| (1..=65535).contains(port))
                .ok_or_else(invalid)?;
            Some(u16::try_from(port).map_err(|_| invalid())?)
        }
    };
    Ok(Some(PiConfig { port }))
}

/// A named Pi.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamedPi {
    pub name: String,
    pub data_dir: PathBuf,
}

/// The named Pis under `home`: every directory under `~/.pi/p3/pis/`, by name.
pub fn named_pis_in(home: &Path) -> Result<Vec<NamedPi>, PiError> {
    let root = pis_root_in(home);
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut pis = Vec::new();
    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !is_pi_name(&name) {
            continue;
        }
        pis.push(NamedPi {
            data_dir: root.join(&name),
            name,
        });
    }
    pis.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(pis)
}

/// The named Pis: every directory under `~/.pi/p3/pis/`, by name.
pub fn named_pis() -> Result<Vec<NamedPi>, PiError> {
    named_pis_in(&crate::files::home_dir()?)
}

/// The Pi's config file.
pub fn config_file(data_dir: &Path) -> PathBuf {
    data_dir.join("pi.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pi_name_is_lower_case_and_bounded() {
        for name in ["a", "two", "a1", "a-b_c", &"a".repeat(32)] {
            assert!(is_pi_name(name), "{name}");
        }
        for name in [
            "",
            "A",
            "Not A Name",
            "-a",
            "_a",
            "a/b",
            &"a".repeat(33),
            "a.b",
        ] {
            assert!(!is_pi_name(name), "{name}");
        }
    }

    #[test]
    fn the_default_pi_keeps_8789_and_another_takes_a_free_port() {
        let root = tempfile::tempdir().expect("temp dir");
        let home = root.path().join("home");
        let default = default_data_dir_in(&home);
        let named = pis_root_in(&home).join("two");
        fs::create_dir_all(&default).expect("default");
        fs::create_dir_all(&named).expect("named");
        assert_eq!(kept_port_in(&home, &default).expect("kept"), (8789, false));
        assert_eq!(kept_port_in(&home, &named).expect("kept"), (0, false));
        keep_port(&named, 9123).expect("keep");
        assert_eq!(kept_port_in(&home, &named).expect("kept"), (9123, true));
        assert_eq!(kept_port_in(&home, &default).expect("kept"), (8789, false));
    }

    #[test]
    fn a_kept_port_is_written_with_tabs() {
        let root = tempfile::tempdir().expect("temp dir");
        let data_dir = root.path().join("pi");
        fs::create_dir_all(&data_dir).expect("data dir");
        keep_port(&data_dir, 9123).expect("keep");
        assert_eq!(
            fs::read_to_string(config_file(&data_dir)).expect("read"),
            "{\n\t\"port\": 9123\n}"
        );
    }

    #[test]
    fn a_config_file_with_a_bad_port_is_refused() {
        let root = tempfile::tempdir().expect("temp dir");
        let data_dir = root.path().join("pi");
        fs::create_dir_all(&data_dir).expect("data dir");
        for bad in ["0", "65536", "\"8789\"", "-1", "1.5"] {
            fs::write(config_file(&data_dir), format!("{{\"port\":{bad}}}")).expect("write");
            assert!(
                matches!(
                    read_config(&data_dir),
                    Err(PiError::InvalidPort { .. }) | Err(PiError::Json { .. })
                ),
                "{bad}"
            );
        }
        fs::write(config_file(&data_dir), "not json").expect("write");
        assert!(matches!(read_config(&data_dir), Err(PiError::Json { .. })));
    }

    #[test]
    fn named_pis_are_the_directories_with_a_pi_name() {
        let root = tempfile::tempdir().expect("temp dir");
        let home = root.path().join("home");
        let pis = pis_root_in(&home);
        fs::create_dir_all(pis.join("two")).expect("two");
        fs::create_dir_all(pis.join("one")).expect("one");
        fs::create_dir_all(pis.join("Not A Name")).expect("bad name");
        fs::write(pis.join("file"), "").expect("file");
        let names = named_pis_in(&home)
            .expect("named")
            .into_iter()
            .map(|pi| pi.name)
            .collect::<Vec<_>>();
        assert_eq!(names, ["one", "two"]);
    }

    #[test]
    fn a_pi_choice_names_one_directory() {
        let root = tempfile::tempdir().expect("temp dir");
        let home = root.path().join("home");
        assert_eq!(
            data_dir_of_in(&home, PiChoice::default()),
            default_data_dir_in(&home)
        );
        assert_eq!(
            data_dir_of_in(
                &home,
                PiChoice {
                    pi: Some("two"),
                    dir: None
                }
            ),
            pis_root_in(&home).join("two")
        );
        assert_eq!(
            data_dir_of_in(
                &home,
                PiChoice {
                    pi: None,
                    dir: Some("/tmp/one")
                }
            ),
            PathBuf::from("/tmp/one")
        );
    }
}
