//! Read-only inventory of the TypeScript reference tree.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use crate::api::v1::FacetKind;

pub type PluginListing = BTreeMap<String, BTreeMap<FacetKind, PathBuf>>;

/// Locate either the p2 app itself or the nearest repository containing it.
pub fn find_app(start: &Path) -> io::Result<Option<PathBuf>> {
    let start = start.canonicalize()?;
    for directory in start.ancestors() {
        for candidate in [directory.to_path_buf(), directory.join("apps/p2")] {
            if candidate.join("src/api/v1/index.ts").is_file()
                && candidate.join("src/hosts/server.ts").is_file()
                && candidate.join("plugins").is_dir()
            {
                return Ok(Some(candidate));
            }
        }
    }
    Ok(None)
}

/// Inventory source facets in alphabetical order. A name belongs to its first root.
pub fn list_roots(roots: &[PathBuf]) -> io::Result<PluginListing> {
    let mut plugins = BTreeMap::new();
    for root in roots {
        for (name, facets) in list_plugins(root)? {
            plugins.entry(name).or_insert(facets);
        }
    }
    Ok(plugins)
}

pub fn list_plugins(root: &Path) -> io::Result<PluginListing> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error),
    };
    let root = root.canonicalize()?;
    let mut plugins = BTreeMap::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().into_string().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "p2 plugin name is not UTF-8")
        })?;
        let directory = root.join(&name);
        let facets = FacetKind::ALL
            .into_iter()
            .filter_map(|kind| {
                ["ts", "tsx"]
                    .into_iter()
                    .map(|extension| directory.join(format!("{}.{extension}", kind.as_str())))
                    .find(|file| file.is_file())
                    .map(|file| (kind, file))
            })
            .collect::<BTreeMap<_, _>>();
        if !facets.is_empty() {
            plugins.insert(name, facets);
        }
    }
    Ok(plugins)
}

/// Resolves the workspace aliases used by upstream, for source inspection only.
pub struct SourceResolver {
    root: PathBuf,
    aliases: Vec<SourceAlias>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct TsConfig {
    compiler_options: CompilerOptions,
}

#[derive(serde::Deserialize)]
struct CompilerOptions {
    paths: indexmap::IndexMap<String, Vec<String>>,
}

struct SourceAlias {
    pattern: String,
    directory: PathBuf,
    replacements: Vec<String>,
}

impl SourceResolver {
    pub fn open(app: &Path) -> io::Result<Self> {
        let app = app.canonicalize()?;
        let root = app
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "missing p2 repository root")
            })?
            .to_owned();
        let mut aliases = Vec::new();
        for (directory, prefix) in [(&root, "@earendil-works/"), (&app, "@p2/")] {
            let config: TsConfig =
                serde_json::from_slice(&fs::read(directory.join("tsconfig.json"))?)?;
            for (pattern, replacements) in config.compiler_options.paths {
                if !pattern.starts_with(prefix) {
                    continue;
                }
                if pattern.bytes().filter(|&b| b == b'*').count() > 1 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("Source runtime does not support multiple wildcards in {pattern}"),
                    ));
                }
                aliases.push(SourceAlias {
                    pattern,
                    directory: directory.clone(),
                    replacements,
                });
            }
        }
        aliases.sort_by_key(|alias| std::cmp::Reverse(alias.pattern.len()));
        Ok(Self { root, aliases })
    }

    pub fn resolve(&self, specifier: &str) -> io::Result<Option<PathBuf>> {
        let mut matched = None;
        for alias in &self.aliases {
            let wildcard = if let Some((prefix, suffix)) = alias.pattern.split_once('*') {
                specifier
                    .strip_prefix(prefix)
                    .and_then(|rest| rest.strip_suffix(suffix))
            } else {
                (specifier == alias.pattern).then_some("")
            };
            let Some(wildcard) = wildcard else { continue };
            matched.get_or_insert(&alias.pattern);
            for replacement in &alias.replacements {
                let path = alias.directory.join(replacement.replacen('*', wildcard, 1));
                let mut candidates = vec![path.clone()];
                match path.extension().and_then(std::ffi::OsStr::to_str) {
                    Some("js") => candidates.push(path.with_extension("ts")),
                    Some("mjs") => candidates.push(path.with_extension("mts")),
                    Some("cjs") => candidates.push(path.with_extension("cts")),
                    Some("ts" | "mts" | "cts" | "json") => {}
                    _ => {
                        let mut appended = path.clone().into_os_string();
                        appended.push(".ts");
                        candidates.push(PathBuf::from(appended));
                        candidates.push(path.join("index.ts"));
                    }
                }
                for candidate in candidates {
                    match candidate.canonicalize() {
                        Ok(file) if file.starts_with(&self.root) && file.is_file() => {
                            return Ok(Some(file));
                        }
                        Ok(_) => {}
                        Err(error)
                            if matches!(
                                error.kind(),
                                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                            ) => {}
                        Err(error) => return Err(error),
                    }
                }
            }
        }
        match matched {
            Some(pattern) => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "Source runtime could not resolve {specifier} through tsconfig path {pattern}"
                ),
            )),
            None => Ok(None),
        }
    }
}
