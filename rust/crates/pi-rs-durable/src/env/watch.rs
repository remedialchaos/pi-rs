use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs::{self, Metadata},
    future::pending,
    io::{self, Read},
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime},
};

use async_trait::async_trait;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use pi_rs_chord::context::Context;
use sha2::{Digest, Sha256};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::{Instant, sleep, sleep_until},
};

use super::{
    FileChange, FileError, FileErrorCode, FileKind, FileWatcher, WatchCallback, WatchMode,
    WatchOptions, WatchTarget, readers::kind_of,
};

const DEBOUNCE: Duration = Duration::from_millis(50);
const SETTLE: Duration = Duration::from_millis(500);
const HASH_MAX_BYTES: u64 = 256 * 1024;
const HASH_RECENT: Duration = Duration::from_secs(5);
const EVENT_QUEUE: usize = 1024;

pub(super) struct ResolvedTarget {
    path: PathBuf,
    recursive: bool,
    hidden: bool,
    names: HashSet<String>,
}

impl ResolvedTarget {
    pub(super) fn new(path: String, target: &WatchTarget) -> Self {
        Self {
            path: PathBuf::from(path),
            recursive: target.recursive,
            hidden: target
                .exclude
                .as_ref()
                .is_some_and(|exclude| exclude.hidden),
            names: target
                .exclude
                .iter()
                .flat_map(|exclude| exclude.names.iter().cloned())
                .collect(),
        }
    }

    fn excludes(&self, name: &std::ffi::OsStr) -> bool {
        let name = name.to_string_lossy();
        (self.hidden && name.starts_with('.')) || self.names.contains(name.as_ref())
    }

    fn covers(&self, path: &Path) -> bool {
        if self.path.starts_with(path) {
            return true;
        }
        let Ok(relative) = path.strip_prefix(&self.path) else {
            return false;
        };
        let mut components = relative.iter();
        let Some(first) = components.next() else {
            return true;
        };
        if self.excludes(first) {
            return false;
        }
        if self.recursive {
            !components.any(|name| self.excludes(name))
        } else {
            components.next().is_none()
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Identity(u64, u64);

impl Identity {
    #[cfg(unix)]
    fn of(metadata: &Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self(metadata.dev(), metadata.ino())
    }

    #[cfg(windows)]
    fn of(metadata: &Metadata) -> Self {
        use std::os::windows::fs::MetadataExt;
        Self(metadata.creation_time(), 0)
    }

    #[cfg(not(any(unix, windows)))]
    fn of(metadata: &Metadata) -> Self {
        use std::time::UNIX_EPOCH;
        let created = metadata
            .created()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .unwrap_or_default();
        Self(created.as_secs(), u64::from(created.subsec_nanos()))
    }
}

#[derive(PartialEq, Eq)]
struct Entry {
    kind: Option<FileKind>,
    identity: Identity,
    size: u64,
    modified: Option<SystemTime>,
    hash: Option<[u8; 32]>,
}

impl Entry {
    fn new(path: &Path, metadata: &Metadata, identity_only: bool, polling: bool) -> Self {
        let identity_only = identity_only || metadata.is_dir();
        let modified = (!identity_only).then(|| metadata.modified().ok()).flatten();
        let size = if identity_only { 0 } else { metadata.len() };
        let hash = if polling
            && metadata.is_file()
            && !identity_only
            && size <= HASH_MAX_BYTES
            && modified.is_some_and(|time| time.elapsed().unwrap_or_default() < HASH_RECENT)
        {
            // A replaced file may be larger by the time it is opened. Keep snapshots bounded anyway.
            fs::File::open(path).ok().and_then(|file| {
                let mut digest = Sha256::new();
                let mut file = file.take(HASH_MAX_BYTES + 1);
                io::copy(&mut file, &mut digest)
                    .ok()
                    .map(|_| digest.finalize().into())
            })
        } else {
            None
        };
        Self {
            kind: kind_of(metadata.file_type()),
            identity: Identity::of(metadata),
            size,
            modified,
            hash,
        }
    }
}

#[derive(Default)]
struct Snapshot {
    entries: HashMap<PathBuf, Entry>,
    // Only directories actually traversed count against the budget, not their ancestors.
    directories: HashSet<PathBuf>,
    // Native watches use lexical paths where possible. Aliases preserve events on symlink targets.
    wanted: HashMap<PathBuf, Identity>,
    aliases: HashMap<PathBuf, Vec<PathBuf>>,
    linked_targets: Vec<(PathBuf, PathBuf)>,
}

impl Snapshot {
    fn record_watch(&mut self, path: &Path, metadata: &Metadata) {
        if self.wanted.contains_key(path) {
            return;
        }
        self.wanted
            .insert(path.to_path_buf(), Identity::of(metadata));
        if let Ok(real) = fs::canonicalize(path)
            && real != path
        {
            self.aliases
                .entry(real)
                .or_default()
                .push(path.to_path_buf());
        }
    }

    fn count_directory(&mut self, path: &Path, max: usize) -> Result<(), FileError> {
        if !self.directories.contains(path) {
            self.directories.insert(path.to_path_buf());
            if self.directories.len() > max {
                return Err(FileError::new(
                    FileErrorCode::Invalid,
                    format!("Watched paths exceed {max} directories"),
                    None,
                ));
            }
        }
        Ok(())
    }
}

fn watch_error(error: io::Error, path: &Path) -> FileError {
    let mut error = FileError::io(error, Some(&path.to_string_lossy()));
    if error.code != FileErrorCode::PermissionDenied {
        error.code = FileErrorCode::Invalid;
    }
    error
}

fn snapshot(targets: &[ResolvedTarget], max: usize, polling: bool) -> Result<Snapshot, FileError> {
    let mut snapshot = Snapshot::default();
    let mut listed = HashMap::new();
    for target in targets {
        for path in target.path.ancestors().skip(1) {
            if snapshot.entries.contains_key(path) {
                continue;
            }
            let Ok(metadata) = fs::symlink_metadata(path) else {
                continue;
            };
            if !polling {
                if metadata.is_dir() {
                    snapshot.record_watch(path, &metadata);
                } else if metadata.is_symlink()
                    && let Ok(actual) = fs::metadata(path)
                    && actual.is_dir()
                {
                    snapshot.record_watch(path, &actual);
                }
            }
            snapshot.entries.insert(
                path.to_path_buf(),
                Entry::new(path, &metadata, true, polling),
            );
        }
        if !polling {
            // Watch referent parents as well, so a dangling link becomes live when its file is recreated.
            let mut link = target.path.clone();
            for _ in 0..40 {
                let Ok(next) = fs::read_link(&link) else {
                    break;
                };
                let Some(parent) = link.parent() else { break };
                let next = super::local::normalize(&parent.join(next));
                if next == link {
                    break;
                }
                for path in next.ancestors().skip(1) {
                    if let Ok(metadata) = fs::metadata(path)
                        && metadata.is_dir()
                    {
                        snapshot.record_watch(path, &metadata);
                    }
                }
                snapshot
                    .linked_targets
                    .push((next.clone(), target.path.clone()));
                link = next;
            }
        }
        // A target may itself be a link; links found below a target are not followed.
        let metadata = match fs::metadata(&target.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                return Err(watch_error(error, &target.path));
            }
            Err(_) => continue,
        };
        snapshot.entries.insert(
            target.path.clone(),
            Entry::new(&target.path, &metadata, false, polling),
        );
        if !polling
            && (metadata.is_dir()
                || (metadata.is_file()
                    && fs::symlink_metadata(&target.path)
                        .is_ok_and(|metadata| metadata.is_symlink())))
        {
            snapshot.record_watch(&target.path, &metadata);
        }
        if !metadata.is_dir() {
            continue;
        }
        // If another target already saw this path, it may still have different exclusions or depth.
        snapshot.count_directory(&target.path, max)?;
        let mut todo = vec![target.path.clone()];
        while let Some(directory) = todo.pop() {
            let entries = match fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(error)
                    if error.kind() == io::ErrorKind::PermissionDenied
                        && directory == target.path =>
                {
                    return Err(watch_error(error, &directory));
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound
                            | io::ErrorKind::NotADirectory
                            | io::ErrorKind::PermissionDenied
                    ) =>
                {
                    continue;
                }
                Err(error) => return Err(watch_error(error, &directory)),
            };
            for entry in entries {
                let entry = entry.map_err(|error| watch_error(error, &directory))?;
                if target.excludes(&entry.file_name()) {
                    continue;
                }
                let path = entry.path();
                let kind = if let Some(kind) = listed.get(&path) {
                    *kind
                } else {
                    let Ok(metadata) = fs::symlink_metadata(&path) else {
                        continue;
                    };
                    let kind = kind_of(metadata.file_type());
                    listed.insert(path.clone(), kind);
                    snapshot
                        .entries
                        .entry(path.clone())
                        .or_insert_with(|| Entry::new(&path, &metadata, false, polling));
                    kind
                };
                if target.recursive && kind == Some(FileKind::Directory) {
                    snapshot.count_directory(&path, max)?;
                    if !polling && let Some(entry) = snapshot.entries.get(&path) {
                        snapshot.wanted.insert(path.clone(), entry.identity);
                    }
                    todo.push(path);
                }
            }
        }
    }
    Ok(snapshot)
}

struct Native {
    watcher: RecommendedWatcher,
    events: mpsc::Receiver<notify::Result<Event>>,
    lost: Arc<AtomicBool>,
    installed: HashMap<PathBuf, Identity>,
}

impl Native {
    fn new() -> notify::Result<Self> {
        let (send, events) = mpsc::channel(EVENT_QUEUE);
        let lost = Arc::new(AtomicBool::new(false));
        let mark_lost = Arc::clone(&lost);
        let watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
            // Watching must not observe the reads that are part of taking its own snapshot.
            if matches!(&event, Ok(event) if matches!(event.kind, EventKind::Access(_)) && !event.need_rescan())
            {
                return;
            }
            if matches!(
                send.try_send(event),
                Err(mpsc::error::TrySendError::Full(_))
            ) {
                mark_lost.store(true, Ordering::Release);
            }
        })?;
        Ok(Self {
            watcher,
            events,
            lost,
            installed: HashMap::new(),
        })
    }

    fn reconcile(&mut self, wanted: &HashMap<PathBuf, Identity>) -> notify::Result<bool> {
        self.installed.retain(|path, old| {
            if wanted.get(path) == Some(old) {
                return true;
            }
            let _ = self.watcher.unwatch(path);
            false
        });
        let mut added = false;
        for (path, identity) in wanted {
            if self.installed.contains_key(path) {
                continue;
            }
            match self.watcher.watch(path, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    self.installed.insert(path.clone(), *identity);
                    added = true;
                }
                Err(error) if transient(&error) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(added)
    }

    fn read_event(
        &mut self,
        event: notify::Result<Event>,
        state: &State,
        changed: &mut BTreeSet<String>,
        overflow: &mut bool,
    ) {
        match event {
            Ok(event) => {
                *overflow |= event.need_rescan();
                *overflow |= event.paths.is_empty();
                for path in event.paths {
                    if let Some(aliases) = state.snapshot.aliases.get(&path) {
                        for alias in aliases {
                            state.report(alias, changed);
                        }
                    }
                    for (real, aliases) in path
                        .ancestors()
                        .skip(1)
                        .filter_map(|parent| state.snapshot.aliases.get_key_value(parent))
                    {
                        if let Ok(suffix) = path.strip_prefix(real) {
                            for alias in aliases {
                                state.report(&alias.join(suffix), changed);
                            }
                        }
                    }
                    for (referent, target) in &state.snapshot.linked_targets {
                        if referent.starts_with(&path) {
                            state.report(target, changed);
                        } else if let Ok(suffix) = path.strip_prefix(referent) {
                            state.report(&target.join(suffix), changed);
                        }
                    }
                    state.report(&path, changed);
                }
            }
            Err(error) => {
                *overflow |= !transient(&error);
                *overflow |= error.paths.is_empty();
                for path in error.paths {
                    if self.installed.remove(&path).is_some() {
                        let _ = self.watcher.unwatch(&path);
                    }
                    state.report(&path, changed);
                }
            }
        }
    }
}

fn transient(error: &notify::Error) -> bool {
    match &error.kind {
        notify::ErrorKind::PathNotFound | notify::ErrorKind::WatchNotFound => true,
        notify::ErrorKind::Io(error) => matches!(
            error.kind(),
            io::ErrorKind::NotFound
                | io::ErrorKind::NotADirectory
                | io::ErrorKind::PermissionDenied
        ),
        _ => false,
    }
}

struct State {
    targets: Arc<[ResolvedTarget]>,
    callback: WatchCallback,
    snapshot: Snapshot,
    max: usize,
    mode: Arc<AtomicBool>,
}

impl State {
    fn report(&self, path: &Path, changed: &mut BTreeSet<String>) {
        for target in self.targets.iter().filter(|target| target.covers(path)) {
            let report = if target.path.starts_with(path) {
                &target.path
            } else {
                path
            };
            changed.insert(report.to_string_lossy().into_owned());
        }
    }

    fn deliver(&mut self, change: FileChange) {
        // A single consumer serializes callbacks. User callbacks cannot stop coverage by panicking.
        let _ = catch_unwind(AssertUnwindSafe(|| (self.callback)(change)));
    }

    async fn sync(
        &mut self,
        native: &mut Option<Native>,
        changed: &mut BTreeSet<String>,
        mut report: bool,
        overflow: &mut bool,
    ) -> Result<bool, FileError> {
        let mut installed = false;
        for _ in 0..10 {
            let targets = Arc::clone(&self.targets);
            let max = self.max;
            let polling = native.is_none();
            let next = tokio::task::spawn_blocking(move || snapshot(&targets, max, polling))
                .await
                .map_err(|error| {
                    FileError::new(FileErrorCode::Unknown, error.to_string(), None)
                })??;
            if report {
                for (path, entry) in &next.entries {
                    if self.snapshot.entries.get(path) != Some(entry) {
                        self.report(path, changed);
                    }
                }
                for path in self.snapshot.entries.keys() {
                    if !next.entries.contains_key(path) {
                        self.report(path, changed);
                    }
                }
            }
            self.snapshot = next;
            let Some(watcher) = native else {
                return Ok(installed);
            };
            match watcher.reconcile(&self.snapshot.wanted) {
                Ok(false) => return Ok(installed),
                Ok(true) => {
                    installed = true;
                    report = true;
                }
                Err(_) => {
                    *native = None;
                    self.mode.store(true, Ordering::Release);
                    *overflow = true;
                    report = true;
                }
            }
        }
        *overflow = true;
        Ok(installed)
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn is_unreliable(targets: &[ResolvedTarget]) -> bool {
    use std::os::unix::ffi::OsStringExt;
    let Ok(mounts) = fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    let mounts: Vec<_> = mounts
        .lines()
        .filter_map(|line| {
            let (attrs, kind) = line.split_once(" - ")?;
            let mount = attrs.split_ascii_whitespace().nth(4)?;
            let kind = kind.split_ascii_whitespace().next()?;
            let mut path = Vec::with_capacity(mount.len());
            let mut bytes = mount.bytes();
            while let Some(byte) = bytes.next() {
                if byte == b'\\' {
                    let digits = [bytes.next()?, bytes.next()?, bytes.next()?];
                    let digits = std::str::from_utf8(&digits).ok()?;
                    path.push(u8::from_str_radix(digits, 8).ok()?);
                } else {
                    path.push(byte);
                }
            }
            let unreliable = matches!(
                kind,
                "nfs"
                    | "nfs4"
                    | "smbfs"
                    | "cifs"
                    | "smb3"
                    | "fuse"
                    | "fuseblk"
                    | "9p"
                    | "lustre"
                    | "gpfs"
                    | "ceph"
                    | "afs"
                    | "kafs"
                    | "sdcardfs"
            ) || kind.starts_with("fuse.");
            Some((
                PathBuf::from(std::ffi::OsString::from_vec(path)),
                unreliable,
            ))
        })
        .collect();
    targets.iter().any(|target| {
        let Some(actual) = target
            .path
            .ancestors()
            .find_map(|path| fs::canonicalize(path).ok())
        else {
            return false;
        };
        mounts
            .iter()
            .filter(|(mount, _)| actual.starts_with(mount))
            .max_by_key(|(path, _)| path.as_os_str().len())
            .is_some_and(|(_, unreliable)| *unreliable)
    })
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn is_unreliable(_targets: &[ResolvedTarget]) -> bool {
    cfg!(windows)
}

struct LocalWatcher {
    mode: Arc<AtomicBool>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

#[async_trait]
impl FileWatcher for LocalWatcher {
    fn mode(&self) -> WatchMode {
        if self.mode.load(Ordering::Acquire) {
            WatchMode::Polling
        } else {
            WatchMode::Native
        }
    }

    async fn stop(&mut self, _context: &Context) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = &mut self.task {
            let _ = task.await;
        }
        self.task = None;
    }
}

pub(super) async fn open(
    targets: Vec<ResolvedTarget>,
    callback: WatchCallback,
    options: WatchOptions,
) -> Result<Box<dyn FileWatcher>, FileError> {
    let targets: Arc<[ResolvedTarget]> = targets.into();
    let prefer_polling = if let Some(mode) = options.mode {
        mode == WatchMode::Polling
    } else {
        let targets = Arc::clone(&targets);
        tokio::task::spawn_blocking(move || is_unreliable(&targets))
            .await
            .map_err(|error| FileError::new(FileErrorCode::Unknown, error.to_string(), None))?
    };
    let mut native = if prefer_polling {
        None
    } else {
        Native::new().ok()
    };
    let mode = Arc::new(AtomicBool::new(native.is_none()));
    let mut overflow = !prefer_polling && native.is_none();
    let mut state = State {
        targets,
        callback,
        mode: Arc::clone(&mode),
        snapshot: Snapshot::default(),
        max: options.max_directories.unwrap_or(10_000),
    };
    let mut changed = BTreeSet::new();
    let installed = state
        .sync(&mut native, &mut changed, false, &mut overflow)
        .await?;
    let (stop, stop_rx) = oneshot::channel();
    let poll_interval = Duration::from_millis(options.poll_interval_ms.unwrap_or(2_000).max(1));
    let task = tokio::spawn(run(
        state,
        native,
        stop_rx,
        poll_interval,
        installed,
        changed,
        overflow,
    ));
    Ok(Box::new(LocalWatcher {
        mode,
        stop: Some(stop),
        task: Some(task),
    }))
}

async fn settle(at: Option<Instant>) {
    if let Some(at) = at {
        sleep_until(at).await
    } else {
        pending().await
    }
}

async fn run(
    mut state: State,
    mut native: Option<Native>,
    mut stop: oneshot::Receiver<()>,
    interval: Duration,
    installed: bool,
    mut changed: BTreeSet<String>,
    mut overflow: bool,
) {
    let mut settle_at = (cfg!(target_os = "macos") && installed).then(|| Instant::now() + SETTLE);
    loop {
        if !matches!(stop.try_recv(), Err(oneshot::error::TryRecvError::Empty)) {
            break;
        }
        if overflow {
            state.deliver(FileChange::Overflow);
            overflow = false;
        }
        if !changed.is_empty() {
            state.deliver(FileChange::Paths {
                paths: std::mem::take(&mut changed).into_iter().collect(),
            });
        }
        if let Some(watcher) = &mut native {
            tokio::select! {
                biased;
                _ = &mut stop => break,
                () = settle(settle_at) => settle_at = None,
                event = watcher.events.recv() => {
                    if let Some(event) = event {
                        watcher.read_event(event, &state, &mut changed, &mut overflow);
                        tokio::select! {
                            biased;
                            _ = &mut stop => break,
                            () = sleep(DEBOUNCE) => {}
                        }
                        for _ in 0..EVENT_QUEUE {
                            let Ok(event) = watcher.events.try_recv() else { break };
                            watcher.read_event(event, &state, &mut changed, &mut overflow);
                        }
                        overflow |= watcher.lost.swap(false, Ordering::AcqRel);
                        if changed.is_empty() && !overflow { continue }
                    } else {
                        native = None;
                        state.mode.store(true, Ordering::Release);
                        overflow = true;
                    }
                }
            }
        } else {
            tokio::select! {
                biased;
                _ = &mut stop => break,
                () = sleep(interval) => {}
            }
        }
        match state
            .sync(&mut native, &mut changed, true, &mut overflow)
            .await
        {
            Ok(true) if cfg!(target_os = "macos") => settle_at = Some(Instant::now() + SETTLE),
            Ok(_) => {}
            Err(error) => {
                if matches!(stop.try_recv(), Err(oneshot::error::TryRecvError::Empty)) {
                    state.deliver(FileChange::Failed { error });
                }
                break;
            }
        }
    }
}
