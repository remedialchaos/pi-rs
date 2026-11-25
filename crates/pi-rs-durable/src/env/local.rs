use std::{
    collections::BTreeMap,
    env, io,
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use ignore::WalkBuilder;
use pi_rs_chord::context::Context;
use tokio::{fs, io::AsyncWriteExt};
use uuid::Uuid;

use super::{
    BinaryReader, CreateDirOptions, DirReader, FileError, FileErrorCode, FileInfo, FileListEntry,
    FileListing, FileSystem, FileWatcher, ListFilesOptions, MAX_SAFE_INTEGER, OpenBinaryOptions,
    RemoveOptions, TempFileOptions, TextLineReader, WatchCallback, WatchOptions, WatchTarget,
    check_abort,
    readers::{LocalBinaryReader, LocalDirReader, LocalTextLineReader, info_of, kind_of},
    shell::ActiveCommands,
    watch,
};

#[derive(Debug, Clone, Default)]
pub struct LocalEnvOptions {
    pub cwd: String,
    pub shell_path: Option<String>,
    pub shell_env: BTreeMap<String, String>,
    pub watch: WatchOptions,
}

pub struct LocalExecutionEnv {
    pub cwd: String,
    pub(super) shell_path: Option<String>,
    pub(super) shell_env: BTreeMap<String, String>,
    watch: WatchOptions,
    pub(super) commands: ActiveCommands,
}

impl LocalExecutionEnv {
    pub fn new(cwd: impl Into<String>) -> Self {
        Self::with_options(LocalEnvOptions {
            cwd: cwd.into(),
            ..LocalEnvOptions::default()
        })
    }

    pub fn with_options(options: LocalEnvOptions) -> Self {
        Self {
            cwd: options.cwd,
            shell_path: options.shell_path,
            shell_env: options.shell_env,
            watch: options.watch,
            commands: ActiveCommands::default(),
        }
    }

    pub(super) fn resolve(&self, path: &str) -> Result<String, FileError> {
        let home = || env::home_dir().unwrap_or_default();
        let path = if path == "~" {
            home()
        } else if let Some(rest) = path
            .strip_prefix("~/")
            .or_else(|| cfg!(windows).then(|| path.strip_prefix("~\\")).flatten())
        {
            // Node's join does not reset to a new root for a later component.
            home().join(rest.trim_start_matches(is_separator))
        } else {
            url_path(path).unwrap_or_else(|| PathBuf::from(path))
        };
        let absolute = if path.is_absolute() {
            path
        } else {
            let mut absolute = PathBuf::from(&self.cwd);
            absolute.push(path);
            if absolute.is_absolute() {
                absolute
            } else {
                std::path::absolute(absolute)
                    .map_err(|error| FileError::io(error, Some(&self.cwd)))?
            }
        };
        Ok(normalize(&absolute).to_string_lossy().into_owned())
    }

    fn file_path(&self, path: &str, context: &Context) -> Result<String, FileError> {
        let path = self.resolve(path)?;
        check_abort(context, Some(&path))?;
        Ok(path)
    }

    async fn write(
        &self,
        path: String,
        content: &[u8],
        append: bool,
        context: &Context,
    ) -> Result<(), FileError> {
        if let Some(parent) = Path::new(&path).parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|error| FileError::io(error, Some(&parent.to_string_lossy())))?;
        }
        check_abort(context, Some(&path))?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .append(append)
            .truncate(!append)
            .open(&path)
            .await
            .map_err(|error| FileError::io(error, Some(&path)))?;
        if append {
            file.write_all(content)
                .await
                .map_err(|error| FileError::io(error, Some(&path)))?;
        } else {
            for chunk in content.chunks(512 * 1024) {
                check_abort(context, Some(&path))?;
                file.write_all(chunk)
                    .await
                    .map_err(|error| FileError::io(error, Some(&path)))?;
            }
        }
        // Tokio's file writes may still be pending until flush.
        file.flush()
            .await
            .map_err(|error| FileError::io(error, Some(&path)))?;
        check_abort(context, Some(&path))
    }
}

fn is_separator(ch: char) -> bool {
    ch == '/' || (cfg!(windows) && ch == '\\')
}

fn url_path(path: &str) -> Option<PathBuf> {
    let rest = path.strip_prefix("file://")?;
    let (host, rest) = rest.split_once('/')?;
    let host = if host.eq_ignore_ascii_case("localhost") {
        ""
    } else {
        host
    };
    if (!cfg!(windows) && !host.is_empty()) || host.contains(['@', ':', '\\', '?', '#']) {
        return None;
    }
    let name = rest.split(['?', '#']).next()?;
    let mut bytes = Vec::with_capacity(name.len() + 1);
    bytes.push(b'/');
    let mut original = name.bytes();
    while let Some(byte) = original.next() {
        if byte == b'%' {
            let upper = char::from(original.next()?).to_digit(16)?;
            let lower = char::from(original.next()?).to_digit(16)?;
            let decoded = ((upper << 4) | lower) as u8;
            if decoded == b'/' || (cfg!(windows) && decoded == b'\\') {
                return None;
            }
            bytes.push(decoded);
        } else {
            bytes.push(byte);
        }
    }
    let name = String::from_utf8(bytes).ok()?;
    if cfg!(windows) {
        if !host.is_empty() {
            return Some(PathBuf::from(format!("//{host}{name}")));
        }
        let drive = name.as_bytes();
        if !(drive.len() >= 3 && drive[1].is_ascii_alphabetic() && drive[2] == b':') {
            return None;
        }
        return Some(PathBuf::from(&name[1..]));
    }
    Some(PathBuf::from(name))
}

pub(super) fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(result.components().next_back(), Some(Component::Normal(_))) {
                    result.pop();
                } else if !result.has_root() {
                    result.push(component);
                }
            }
            Component::Normal(name) => {
                // Push a raw component without re-parsing a later Windows drive prefix.
                if result.as_os_str().is_empty() {
                    result.push(name);
                    continue;
                }
                let needs_separator = !result
                    .as_os_str()
                    .as_encoded_bytes()
                    .last()
                    .is_some_and(|byte| is_separator(char::from(*byte)))
                    && !matches!(result.components().next_back(), Some(Component::Prefix(_)));
                if needs_separator {
                    result
                        .as_mut_os_string()
                        .push(std::path::MAIN_SEPARATOR_STR);
                }
                result.as_mut_os_string().push(name);
            }
            _ => result.push(component),
        }
    }
    result
}

fn join_paths(parts: &[&str]) -> String {
    let joined = parts
        .iter()
        .copied()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(std::path::MAIN_SEPARATOR_STR);
    let mut result = normalize(Path::new(&joined)).to_string_lossy().into_owned();
    if result.is_empty() {
        result.push('.');
    }
    if joined.ends_with(is_separator) && !result.ends_with(is_separator) {
        result.push(std::path::MAIN_SEPARATOR);
    }
    result
}

fn canonical_display(path: PathBuf) -> String {
    let path = path.to_string_lossy();
    #[cfg(windows)]
    if let Some(path) = path.strip_prefix(r"\\?\") {
        return match path.strip_prefix(r"UNC\") {
            Some(path) => format!(r"\\{path}"),
            None => path.to_owned(),
        };
    }
    path.into_owned()
}

#[async_trait]
impl FileSystem for LocalExecutionEnv {
    fn id(&self) -> &str {
        "node:local"
    }

    fn cwd(&self) -> &str {
        &self.cwd
    }

    async fn absolute_path(&self, path: &str, _context: &Context) -> Result<String, FileError> {
        self.resolve(path)
    }

    async fn join_path(&self, parts: &[&str], _context: &Context) -> Result<String, FileError> {
        Ok(join_paths(parts))
    }

    async fn read_text_file(&self, path: &str, context: &Context) -> Result<String, FileError> {
        let bytes = self.read_binary_file(path, context).await?;
        // Node's fs.readFile("utf8"), unlike its streaming TextDecoder, retains the BOM.
        Ok(String::from_utf8(bytes)
            .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned()))
    }

    async fn open_text_line_reader(
        &self,
        path: &str,
        context: &Context,
    ) -> Result<Box<dyn TextLineReader>, FileError> {
        let path = self.file_path(path, context)?;
        let file = fs::File::open(&path)
            .await
            .map_err(|error| FileError::io(error, Some(&path)))?;
        check_abort(context, Some(&path))?;
        Ok(Box::new(LocalTextLineReader::new(file, path)))
    }

    async fn read_binary_file(&self, path: &str, context: &Context) -> Result<Vec<u8>, FileError> {
        let path = self.file_path(path, context)?;
        context
            .await_with(fs::read(&path))
            .await
            .map_err(|_| FileError::at(FileErrorCode::Aborted, "aborted", &path))?
            .map_err(|error| FileError::io(error, Some(&path)))
    }

    async fn open_binary_reader(
        &self,
        path: &str,
        options: Option<OpenBinaryOptions>,
        context: &Context,
    ) -> Result<Box<dyn BinaryReader>, FileError> {
        let path = self.resolve(path)?;
        Ok(Box::new(
            LocalBinaryReader::open(
                path,
                options.is_some_and(|options| options.no_follow),
                context,
            )
            .await?,
        ))
    }

    async fn write_file(
        &self,
        path: &str,
        content: &[u8],
        context: &Context,
    ) -> Result<(), FileError> {
        self.write(self.file_path(path, context)?, content, false, context)
            .await
    }

    async fn append_file(
        &self,
        path: &str,
        content: &[u8],
        context: &Context,
    ) -> Result<(), FileError> {
        self.write(self.file_path(path, context)?, content, true, context)
            .await
    }

    async fn truncate_file(
        &self,
        path: &str,
        size: u64,
        context: &Context,
    ) -> Result<(), FileError> {
        let path = self.file_path(path, context)?;
        if size > MAX_SAFE_INTEGER {
            return Err(FileError::at(
                FileErrorCode::Invalid,
                "File size must be a non-negative safe integer",
                &path,
            ));
        }
        let file = open_for_write(&path).await?;
        file.set_len(size)
            .await
            .map_err(|error| FileError::io(error, Some(&path)))?;
        check_abort(context, Some(&path))
    }

    async fn flush_file(&self, path: &str, context: &Context) -> Result<(), FileError> {
        let path = self.file_path(path, context)?;
        let file = open_for_write(&path).await?;
        file.sync_all()
            .await
            .map_err(|error| FileError::io(error, Some(&path)))?;
        check_abort(context, Some(&path))
    }

    async fn rename_file(
        &self,
        source: &str,
        destination: &str,
        context: &Context,
    ) -> Result<(), FileError> {
        let source = self.resolve(source)?;
        let destination = self.file_path(destination, context)?;
        fs::rename(&source, destination)
            .await
            .map_err(|error| FileError::io(error, Some(&source)))
    }

    async fn file_info(&self, path: &str, context: &Context) -> Result<FileInfo, FileError> {
        let path = self.file_path(path, context)?;
        let stats = fs::symlink_metadata(&path)
            .await
            .map_err(|error| FileError::io(error, Some(&path)))?;
        info_of(&path, &stats)
    }

    async fn list_dir(&self, path: &str, context: &Context) -> Result<Vec<FileInfo>, FileError> {
        let mut reader = self.open_dir_reader(path, context).await?;
        let result = reader
            .next(MAX_SAFE_INTEGER, context)
            .await
            .map(|page| page.entries);
        reader.close(context).await;
        result
    }

    async fn list_files(
        &self,
        path: &str,
        options: Option<ListFilesOptions>,
        context: &Context,
    ) -> Result<FileListing, FileError> {
        let path = self.file_path(path, context)?;
        let context = context.clone();
        tokio::task::spawn_blocking(move || list_tree(&path, options.unwrap_or_default(), &context))
            .await
            .map_err(|error| FileError::new(FileErrorCode::Unknown, error.to_string(), None))?
    }

    async fn open_dir_reader(
        &self,
        path: &str,
        context: &Context,
    ) -> Result<Box<dyn DirReader>, FileError> {
        let path = self.file_path(path, context)?;
        let dir = fs::read_dir(&path)
            .await
            .map_err(|error| FileError::io(error, Some(&path)))?;
        check_abort(context, Some(&path))?;
        Ok(Box::new(LocalDirReader::new(dir, path)))
    }

    async fn watch(
        &self,
        targets: &[WatchTarget],
        on_change: WatchCallback,
        context: &Context,
    ) -> Result<Box<dyn FileWatcher>, FileError> {
        check_abort(context, None)?;
        let targets = targets
            .iter()
            .map(|target| {
                Ok(watch::ResolvedTarget::new(
                    self.resolve(&target.path)?,
                    target,
                ))
            })
            .collect::<Result<Vec<_>, FileError>>()?;
        let mut watcher = watch::open(targets, on_change, self.watch).await?;
        if let Err(error) = check_abort(context, None) {
            watcher.stop(context).await;
            return Err(error);
        }
        Ok(watcher)
    }

    async fn canonical_path(&self, path: &str, context: &Context) -> Result<String, FileError> {
        let path = self.file_path(path, context)?;
        fs::canonicalize(&path)
            .await
            .map(canonical_display)
            .map_err(|error| FileError::io(error, Some(&path)))
    }

    async fn create_dir(
        &self,
        path: &str,
        options: Option<CreateDirOptions>,
        context: &Context,
    ) -> Result<(), FileError> {
        let path = self.file_path(path, context)?;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(options.unwrap_or_default().recursive);
        builder
            .create(&path)
            .await
            .map_err(|error| FileError::io(error, Some(&path)))
    }

    async fn remove(
        &self,
        path: &str,
        options: Option<RemoveOptions>,
        context: &Context,
    ) -> Result<(), FileError> {
        let path = self.file_path(path, context)?;
        let options = options.unwrap_or_default();
        let result = async {
            let file_type = fs::symlink_metadata(&path).await?.file_type();
            if file_type.is_dir() {
                if !options.recursive {
                    return Err(io::Error::from(io::ErrorKind::IsADirectory));
                }
                fs::remove_dir_all(&path).await
            } else {
                #[cfg(windows)]
                {
                    use std::os::windows::fs::FileTypeExt;
                    if file_type.is_symlink_dir() {
                        return fs::remove_dir(&path).await;
                    }
                }
                fs::remove_file(&path).await
            }
        }
        .await;
        match result {
            Err(error) if options.force && error.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other.map_err(|error| FileError::io(error, Some(&path))),
        }
    }

    async fn create_temp_dir(
        &self,
        prefix: Option<&str>,
        context: &Context,
    ) -> Result<String, FileError> {
        check_abort(context, None)?;
        let temp_dir = env::temp_dir();
        let prefix = join_paths(&[&temp_dir.to_string_lossy(), prefix.unwrap_or("tmp-")]);
        let path = format!("{prefix}{}", Uuid::new_v4().simple());
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        builder.mode(0o700);
        builder
            .create(&path)
            .await
            .map_err(|error| FileError::io(error, Some(&path)))?;
        Ok(path)
    }

    async fn create_temp_file(
        &self,
        options: Option<TempFileOptions<'_>>,
        context: &Context,
    ) -> Result<String, FileError> {
        let dir = self.create_temp_dir(Some("tmp-"), context).await?;
        let options = options.unwrap_or_default();
        let path = join_paths(&[
            &dir,
            &format!(
                "{}{}{}",
                options.prefix.unwrap_or_default(),
                Uuid::new_v4(),
                options.suffix.unwrap_or_default()
            ),
        ]);
        if let Err(error) = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
        {
            let _ = fs::remove_dir(&dir).await;
            return Err(FileError::io(error, Some(&path)));
        }
        Ok(path)
    }
}

async fn open_for_write(path: &str) -> Result<fs::File, FileError> {
    #[cfg(windows)]
    if fs::metadata(path)
        .await
        .is_ok_and(|metadata| metadata.is_dir())
    {
        return Err(FileError::at(
            FileErrorCode::IsDirectory,
            "Is a directory",
            path,
        ));
    }
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .await
        .map_err(|error| FileError::io(error, Some(path)))
}

fn list_tree(
    path: &str,
    options: ListFilesOptions,
    context: &Context,
) -> Result<FileListing, FileError> {
    const MAX_BYTES: u64 = (16 * 1024 - 64) * 1024;
    for (name, bound) in [
        ("maxEntries", options.max_entries),
        ("maxBytes", options.max_bytes),
        ("timeoutMs", options.timeout_ms),
    ] {
        if bound.is_some_and(|bound| bound > MAX_SAFE_INTEGER) {
            return Err(FileError::at(
                FileErrorCode::Invalid,
                format!("{name} must be a non-negative safe integer"),
                path,
            ));
        }
    }
    let max_entries = options.max_entries.unwrap_or(50_000);
    let max_bytes = options.max_bytes.unwrap_or(4 * 1024 * 1024).min(MAX_BYTES);
    let timeout = Duration::from_millis(options.timeout_ms.unwrap_or(10_000));
    check_abort(context, Some(path))?;
    let root = Path::new(path);
    let stats = std::fs::metadata(root).map_err(|error| FileError::io(error, Some(path)))?;
    if !stats.is_dir() {
        return Err(FileError::at(
            FileErrorCode::NotDirectory,
            "Not a directory",
            path,
        ));
    }
    let gitignore = options.gitignore.unwrap_or(true);
    let walk = WalkBuilder::new(root)
        .hidden(!options.hidden.unwrap_or(true))
        .ignore(false)
        .parents(gitignore)
        .git_ignore(gitignore)
        .git_exclude(gitignore)
        .git_global(gitignore)
        .require_git(true)
        .follow_links(false)
        .current_dir(root)
        .sort_by_file_name(|left, right| left.cmp(right))
        .filter_entry(move |entry| {
            let name = entry.file_name();
            name != ".git"
                && !options
                    .exclude
                    .iter()
                    .any(|excluded| name == excluded.as_str())
        })
        .build();
    let start = Instant::now();
    let mut entries = Vec::new();
    let mut bytes = 0;
    let mut truncated = false;
    for entry in walk {
        check_abort(context, Some(path))?;
        let Ok(entry) = entry else { continue };
        if entry.depth() == 0 {
            continue;
        }
        let Some(kind) = entry.file_type().and_then(kind_of) else {
            continue;
        };
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        let mut name = String::new();
        for part in relative.iter() {
            if !name.is_empty() {
                name.push('/');
            }
            name.push_str(&part.to_string_lossy());
        }
        let size = name.len() as u64 + 2;
        if entries.len() as u64 >= max_entries
            || bytes + size > max_bytes
            || start.elapsed() > timeout
        {
            truncated = true;
            break;
        }
        bytes += size;
        entries.push(FileListEntry { path: name, kind });
    }
    check_abort(context, Some(path))?;
    Ok(FileListing { entries, truncated })
}
