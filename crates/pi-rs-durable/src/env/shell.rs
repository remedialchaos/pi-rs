use std::{
    any::Any,
    collections::HashMap,
    env,
    future::pending,
    io,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{Mutex, MutexGuard},
    time::Duration,
};

use async_trait::async_trait;
use pi_rs_chord::context::{AbortSignal, Cancel, Context};
use tokio::{
    fs,
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    time::{Instant, sleep_until},
};

use super::{
    ExecutionError, ExecutionErrorCode, FileSystem, LocalExecutionEnv, RemoveOptions, Shell,
    ShellCommand, ShellExecOptions, ShellExecResult, ShellOutputCallback, ShellOutputInfo,
    ShellOutputStream, ShellSpillOptions, StreamDecoder, TempFileOptions,
};

const MAX_TIMEOUT_SECONDS: f64 = 2_147_483.647;
const STDIO_GRACE: Duration = Duration::from_millis(100);
const PIPE_CHUNK: usize = 16 * 1024;

#[derive(Default)]
pub(super) struct ActiveCommands(Mutex<HashMap<u32, Cancel>>);

impl ActiveCommands {
    fn lock(&self) -> MutexGuard<'_, HashMap<u32, Cancel>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn add(&self, pid: u32) -> (ActiveCommand<'_>, AbortSignal) {
        let (signal, cancel) = AbortSignal::new();
        self.lock().insert(pid, cancel);
        (
            ActiveCommand {
                commands: self,
                pid,
                armed: true,
            },
            signal,
        )
    }

    fn cleanup(&self) {
        let mut commands = self.lock();
        for (_, cancel) in commands.drain() {
            cancel.cancel("cleanup");
        }
    }
}

struct ActiveCommand<'a> {
    commands: &'a ActiveCommands,
    pid: u32,
    armed: bool,
}

impl ActiveCommand<'_> {
    async fn terminate(&mut self, child: &mut Child) {
        if self.armed {
            let pid = self.pid;
            if !tokio::task::spawn_blocking(move || kill_tree(pid))
                .await
                .unwrap_or(false)
            {
                let _ = child.start_kill();
            }
            self.armed = false;
        }
    }
}

impl Drop for ActiveCommand<'_> {
    fn drop(&mut self) {
        self.commands.lock().remove(&self.pid);
        if self.armed {
            kill_tree(self.pid);
        }
    }
}

#[cfg(unix)]
fn kill_tree(pid: u32) -> bool {
    // The child is its own process-group leader. Use the platform's kill utility rather than FFI.
    let group = format!("-{pid}");
    #[cfg(target_os = "android")]
    let utility = "/system/bin/kill";
    #[cfg(not(target_os = "android"))]
    let utility = "/bin/kill";
    std::process::Command::new(utility)
        .args(["-KILL", "--", &group])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(windows)]
fn kill_tree(pid: u32) -> bool {
    use std::os::windows::process::CommandExt;
    let root = env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    std::process::Command::new(PathBuf::from(root).join(r"System32\taskkill.exe"))
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(not(any(unix, windows)))]
fn kill_tree(_pid: u32) -> bool {
    false
}

async fn find_executable_on_path(executable: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let mut where_command = Command::new("where.exe");
        where_command
            .arg(executable)
            .creation_flags(0x0800_0000)
            .kill_on_drop(true)
            .stdin(Stdio::null());
        let output = tokio::time::timeout(Duration::from_secs(5), where_command.output())
            .await
            .ok()?
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let first = std::str::from_utf8(&output.stdout)
            .ok()?
            .trim()
            .lines()
            .next()?;
        let path = PathBuf::from(first);
        fs::metadata(&path).await.ok()?;
        Some(path)
    }
    #[cfg(not(windows))]
    {
        let path = env::var_os("PATH")?;
        for dir in env::split_paths(&path) {
            let candidate = dir.join(executable);
            let Ok(metadata) = fs::metadata(&candidate).await else {
                continue;
            };
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o111 == 0 {
                    continue;
                }
            }
            if metadata.is_file() {
                return Some(candidate);
            }
        }
        None
    }
}

#[cfg(any(windows, test))]
// Git for Windows reports its POSIX shell as `<root>/usr/bin/sh.exe`.
const GIT_FOR_WINDOWS_SHELL_SUFFIX: &str = r"usr\bin\sh.exe";

#[cfg(any(windows, test))]
fn normalize_win32_path(raw: &str) -> String {
    let slash_normalized = raw.replace('/', "\\");
    let (prefix, rest, is_absolute) = if let Some(after) = slash_normalized.strip_prefix(r"\\") {
        (r"\\".to_owned(), after, true)
    } else if slash_normalized
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphabetic)
        && slash_normalized.as_bytes().get(1) == Some(&b':')
    {
        let drive = &slash_normalized[..2];
        let after = &slash_normalized[2..];
        if let Some(after) = after.strip_prefix('\\') {
            (format!(r"{drive}\"), after, true)
        } else {
            (drive.to_owned(), after, false)
        }
    } else if let Some(after) = slash_normalized.strip_prefix('\\') {
        (r"\".to_owned(), after, true)
    } else {
        (String::new(), slash_normalized.as_str(), false)
    };
    let trailing_sep = !rest.is_empty() && rest.ends_with('\\');
    let mut parts: Vec<&str> = Vec::new();
    for segment in rest.split('\\') {
        match segment {
            "" | "." => {}
            ".." => {
                if matches!(parts.last(), Some(&last) if last != "..") {
                    parts.pop();
                } else if !is_absolute {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let mut out = prefix;
    out.push_str(&parts.join(r"\"));
    if trailing_sep && !parts.is_empty() {
        out.push('\\');
    }
    if out.is_empty() { ".".to_owned() } else { out }
}

#[cfg(any(windows, test))]
fn git_bash_from_git_shell_path(git_shell_path: &str) -> Option<String> {
    let git_shell = normalize_win32_path(git_shell_path.trim());
    let expected = format!(r"\{GIT_FOR_WINDOWS_SHELL_SUFFIX}");
    if !git_shell.to_ascii_lowercase().ends_with(&expected) {
        return None;
    }
    let root = &git_shell[..git_shell.len() - GIT_FOR_WINDOWS_SHELL_SUFFIX.len()];
    Some(format!(r"{root}bin\bash.exe"))
}

#[cfg(windows)]
async fn run_git_var_shell_path(git: PathBuf) -> Option<String> {
    let mut command = Command::new(git);
    command
        .args(["var", "GIT_SHELL_PATH"])
        .creation_flags(0x0800_0000)
        .kill_on_drop(true)
        .stdin(Stdio::null());
    let output = tokio::time::timeout(Duration::from_secs(5), command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

#[cfg(any(windows, test))]
async fn resolve_windows_shell_with<FEnv, FExists, FFind, FGit, FutExists, FutFind, FutGit>(
    mut get_env: FEnv,
    mut exists: FExists,
    mut find_executable: FFind,
    mut git_var_shell_path: FGit,
) -> Result<PathBuf, ExecutionError>
where
    FEnv: FnMut(&str) -> Option<std::ffi::OsString>,
    FExists: FnMut(PathBuf) -> FutExists,
    FutExists: std::future::Future<Output = bool>,
    FFind: FnMut(&'static str) -> FutFind,
    FutFind: std::future::Future<Output = Option<PathBuf>>,
    FGit: FnMut(PathBuf) -> FutGit,
    FutGit: std::future::Future<Output = Option<String>>,
{
    let mut searched = String::new();
    for key in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(path) = get_env(key) {
            let candidate = PathBuf::from(path).join(r"Git\bin\bash.exe");
            if exists(candidate.clone()).await {
                return Ok(candidate);
            }
            searched.push_str("\n  ");
            searched.push_str(&candidate.to_string_lossy());
        }
    }
    // Must precede the generic bash.exe lookup, which usually finds the WSL launcher in System32 first.
    if let Some(git) = find_executable("git.exe").await
        && let Some(stdout) = git_var_shell_path(git).await
        && let Some(bash) = git_bash_from_git_shell_path(&stdout)
    {
        let candidate = PathBuf::from(bash);
        if exists(candidate.clone()).await {
            return Ok(candidate);
        }
    }
    if let Some(path) = find_executable("bash.exe").await {
        return Ok(path);
    }
    Err(ExecutionError::new(
        ExecutionErrorCode::ShellUnavailable,
        format!(
            "No bash shell found. Options:\n  1. Install Git for Windows: https://git-scm.com/download/win\n  2. Add your bash to PATH (Cygwin, MSYS2, etc.)\n  3. Configure an explicit shellPath\n\nSearched Git Bash in:{searched}\n  the Git for Windows installation of git.exe on PATH"
        ),
    ))
}

async fn shell_path(configured: Option<&str>) -> Result<PathBuf, ExecutionError> {
    if let Some(path) = configured.filter(|path| !path.is_empty()) {
        return if fs::metadata(path).await.is_ok() {
            Ok(PathBuf::from(path))
        } else {
            Err(ExecutionError::new(
                ExecutionErrorCode::ShellUnavailable,
                format!("Custom shell path not found: {path}"),
            ))
        };
    }
    #[cfg(windows)]
    {
        resolve_windows_shell_with(
            |key| env::var_os(key),
            |path| async move { fs::metadata(&path).await.is_ok() },
            find_executable_on_path,
            run_git_var_shell_path,
        )
        .await
    }
    #[cfg(not(windows))]
    {
        if fs::metadata("/bin/bash").await.is_ok() {
            return Ok(PathBuf::from("/bin/bash"));
        }
        if let Some(path) = find_executable_on_path("bash").await {
            return Ok(path);
        }
        Ok(PathBuf::from("sh"))
    }
}

#[cfg(windows)]
fn legacy_wsl_launcher(path: &Path) -> bool {
    fn windows_path(path: &Path) -> String {
        path.components()
            .collect::<PathBuf>()
            .to_string_lossy()
            .replace('/', "\\")
            .to_ascii_lowercase()
    }
    let normalized = windows_path(path);
    if let Some(root) = env::var_os("SystemRoot") {
        let root = Path::new(&root);
        if !root.is_absolute() {
            return false;
        }
        [r"System32\bash.exe", r"Sysnative\bash.exe"]
            .into_iter()
            .any(|launcher| normalized == windows_path(&root.join(launcher)))
    } else {
        let bytes = normalized.as_bytes();
        bytes.first().is_some_and(u8::is_ascii_alphabetic)
            && normalized.get(1..).is_some_and(|path| {
                matches!(
                    path,
                    r":\windows\system32\bash.exe" | r":\windows\sysnative\bash.exe"
                )
            })
    }
}

#[cfg(not(windows))]
fn legacy_wsl_launcher(_path: &Path) -> bool {
    false
}

fn timeout_duration(timeout: Option<f64>) -> Result<Option<Duration>, ExecutionError> {
    let Some(timeout) = timeout else {
        return Ok(None);
    };
    if !timeout.is_finite() || timeout <= 0.0 {
        return Err(ExecutionError::new(
            ExecutionErrorCode::Timeout,
            "Invalid timeout: must be a finite number of seconds",
        ));
    }
    if timeout > MAX_TIMEOUT_SECONDS {
        return Err(ExecutionError::new(
            ExecutionErrorCode::Timeout,
            format!("Invalid timeout: maximum is {MAX_TIMEOUT_SECONDS} seconds"),
        ));
    }
    Ok(Some(Duration::from_millis(
        (timeout * 1000.0).max(1.0) as u64
    )))
}

fn spawn_error(error: io::Error) -> ExecutionError {
    ExecutionError::new(ExecutionErrorCode::SpawnError, error.to_string()).caused_by(error)
}

fn aborted() -> ExecutionError {
    ExecutionError::new(ExecutionErrorCode::Aborted, "aborted")
}

fn timed_out(timeout: Option<f64>) -> ExecutionError {
    ExecutionError::new(
        ExecutionErrorCode::Timeout,
        format!("timeout:{}", timeout.unwrap_or_default()),
    )
}

async fn deadline(at: Option<Instant>) {
    if let Some(at) = at {
        sleep_until(at).await
    } else {
        pending().await
    }
}

async fn cancellation(signal: Option<&AbortSignal>) {
    if let Some(signal) = signal {
        signal.cancelled().await;
    } else {
        pending().await
    }
}

struct Pipe<R> {
    reader: Option<R>,
    bytes: Box<[u8]>,
    decoder: StreamDecoder,
}

impl<R: AsyncRead + Unpin> Pipe<R> {
    fn new(reader: Option<R>) -> Self {
        Self {
            reader,
            bytes: vec![0; PIPE_CHUNK].into_boxed_slice(),
            decoder: StreamDecoder::new(),
        }
    }

    async fn read(&mut self) -> io::Result<usize> {
        if let Some(reader) = &mut self.reader {
            reader.read(&mut self.bytes).await
        } else {
            pending().await
        }
    }

    fn done(&self) -> bool {
        self.reader.is_none()
    }

    fn output(&mut self, count: usize, scratch: &mut String, output: bool) -> &[u8] {
        let bytes = &self.bytes[..count];
        if output && count != 0 {
            scratch.clear();
            self.decoder.decode_into(Some(bytes), scratch);
        }
        if count == 0 {
            self.reader = None;
        }
        bytes
    }
}

struct Spill {
    options: ShellSpillOptions,
    prefix: Vec<u8>,
    newlines: usize,
    file: Option<(String, fs::File)>,
}

impl Spill {
    async fn feed(
        &mut self,
        bytes: &[u8],
        env: &LocalExecutionEnv,
        context: &Context,
    ) -> Result<(), ExecutionError> {
        if bytes.is_empty() {
            return Ok(());
        }
        if let Some((_, file)) = &mut self.file {
            return file.write_all(bytes).await.map_err(spill_error);
        }
        self.newlines += bytes.iter().filter(|byte| **byte == b'\n').count();
        let lines = self.newlines + usize::from(bytes.last() != Some(&b'\n'));
        if self.prefix.len().saturating_add(bytes.len()) <= self.options.after_bytes
            && lines <= self.options.after_lines
        {
            self.prefix.extend_from_slice(bytes);
            return Ok(());
        }
        let path = env
            .create_temp_file(
                Some(TempFileOptions {
                    prefix: Some("pi-output-"),
                    suffix: Some(".log"),
                }),
                &context.without_abort_signal(),
            )
            .await
            .map_err(spill_error)?;
        let file = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .await
            .map_err(spill_error)?;
        let (_, file) = self.file.insert((path, file));
        file.write_all(&self.prefix).await.map_err(spill_error)?;
        self.prefix = Vec::new();
        file.write_all(bytes).await.map_err(spill_error)
    }

    async fn finish(&mut self) -> Result<(), ExecutionError> {
        if let Some((_, file)) = &mut self.file {
            file.flush().await.map_err(spill_error)?;
        }
        Ok(())
    }

    fn into_path(mut self) -> Option<String> {
        self.file.take().map(|(path, _)| path)
    }
}

fn spill_error(error: impl std::error::Error + Send + Sync + 'static) -> ExecutionError {
    ExecutionError::new(
        ExecutionErrorCode::Unknown,
        format!("Failed to preserve complete shell output: {error}"),
    )
    .caused_by(error)
}

fn panic_message(panic: &(dyn Any + Send)) -> &str {
    panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("Shell output callback panicked")
}

fn emit(
    callback: Option<&mut ShellOutputCallback<'_>>,
    text: &str,
    stream: ShellOutputStream,
    context: &Context,
) -> Result<(), ExecutionError> {
    let Some(callback) = callback.filter(|_| !text.is_empty()) else {
        return Ok(());
    };
    match catch_unwind(AssertUnwindSafe(|| {
        callback(
            text,
            context,
            ShellOutputInfo {
                stream,
                skipped: None,
            },
        )
    })) {
        Ok(result) => result.map_err(|error| {
            ExecutionError::new(ExecutionErrorCode::CallbackError, error.message.clone())
                .caused_by(error)
        }),
        Err(panic) => Err(ExecutionError::new(
            ExecutionErrorCode::CallbackError,
            panic_message(&*panic),
        )),
    }
}

fn exit_code(status: ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status
            .code()
            .unwrap_or_else(|| status.signal().map_or(1, |signal| 128 + signal))
    }
    #[cfg(not(unix))]
    status.code().unwrap_or(1)
}

enum Event {
    Cancelled,
    TimedOut,
    Cleanup,
    Idle,
    Exit(io::Result<ExitStatus>),
    Stdout(io::Result<usize>),
    Stderr(io::Result<usize>),
    Stdin,
}

#[async_trait]
impl Shell for LocalExecutionEnv {
    async fn exec(
        &self,
        command: ShellCommand<'_>,
        options: Option<ShellExecOptions<'_>>,
        context: &Context,
    ) -> Result<ShellExecResult, ExecutionError> {
        if context.is_aborted() {
            return Err(aborted());
        }
        let options = options.unwrap_or_default();
        let timeout = timeout_duration(options.timeout)?;
        let cwd = self
            .resolve(options.cwd.filter(|cwd| !cwd.is_empty()).unwrap_or("."))
            .map_err(|error| {
                ExecutionError::new(ExecutionErrorCode::SpawnError, error.message.clone())
                    .caused_by(error)
            })?;
        let (mut command, stdin_script) = match command {
            ShellCommand::Script(script) => {
                let path = shell_path(self.shell_path.as_deref()).await?;
                let from_stdin = legacy_wsl_launcher(&path);
                let mut command = Command::new(path);
                if from_stdin {
                    command.arg("-s");
                } else {
                    command.args(["-c", script]);
                }
                (command, from_stdin.then_some(script))
            }
            ShellCommand::Argv(argv) => {
                let (program, arguments) = argv.split_first().ok_or_else(|| {
                    ExecutionError::new(
                        ExecutionErrorCode::SpawnError,
                        "Empty argv: no program to run",
                    )
                })?;
                let mut command = Command::new(program);
                command.args(arguments);
                (command, None)
            }
        };
        if let Err(error) = fs::metadata(&cwd).await {
            return Err(ExecutionError::new(
                ExecutionErrorCode::SpawnError,
                format!("Working directory does not exist: {cwd}\nCannot execute bash commands."),
            )
            .caused_by(error));
        }
        if options.inherit_env.unwrap_or(true) {
            command.envs(&self.shell_env);
        } else {
            command.env_clear();
        }
        if let Some(env) = options.env {
            command.envs(env);
        }
        command
            .current_dir(cwd)
            .stdin(if stdin_script.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(windows)]
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        let mut child = command.spawn().map_err(spawn_error)?;
        let pid = child.id().ok_or_else(|| {
            ExecutionError::new(ExecutionErrorCode::SpawnError, "Child process has no id")
        })?;
        let (mut active, cleanup) = self.commands.add(pid);
        let mut stdout = Pipe::new(child.stdout.take());
        let mut stderr = Pipe::new(child.stderr.take());
        let stdin = child.stdin.take();
        let stdin_work = async move {
            if let (Some(mut stdin), Some(script)) = (stdin, stdin_script) {
                let _ = stdin.write_all(script.as_bytes()).await;
            }
        };
        tokio::pin!(stdin_work);
        let mut stdin_pending = stdin_script.is_some();
        let request_signal = context.abort_signal();
        let mut timeout_at = timeout.map(|duration| Instant::now() + duration);
        let mut idle_at = None;
        let mut status = None;
        let mut interrupted = false;
        let mut expired = false;
        let mut stopped = false;
        let mut callback = options.on_output;
        let mut callback_error = None;
        let mut spill_error = None;
        let mut spill = options.spill.map(|options| Spill {
            options,
            prefix: Vec::new(),
            newlines: 0,
            file: None,
        });
        let mut scratch = String::new();
        while status.is_none() || !(stdout.done() && stderr.done()) {
            let event = tokio::select! {
                biased;
                () = deadline(timeout_at) => Event::TimedOut,
                () = cancellation(request_signal.as_ref()), if !interrupted => Event::Cancelled,
                _ = cleanup.cancelled(), if !stopped => Event::Cleanup,
                () = deadline(idle_at) => Event::Idle,
                event = async {
                    // When both pipes are ready, avoid starving either of them.
                    tokio::select! {
                        result = child.wait(), if status.is_none() => Event::Exit(result),
                        result = stdout.read(), if !stdout.done() => Event::Stdout(result),
                        result = stderr.read(), if !stderr.done() => Event::Stderr(result),
                        () = &mut stdin_work, if stdin_pending => Event::Stdin,
                    }
                } => event,
            };
            let (raw, stream) = match event {
                Event::TimedOut => {
                    timeout_at = None;
                    expired = true;
                    active.terminate(&mut child).await;
                    continue;
                }
                Event::Cancelled => {
                    interrupted = true;
                    active.terminate(&mut child).await;
                    continue;
                }
                Event::Cleanup => {
                    stopped = true;
                    active.terminate(&mut child).await;
                    continue;
                }
                Event::Exit(result) => {
                    status = Some(result.map_err(spawn_error)?);
                    idle_at = Some(Instant::now() + STDIO_GRACE);
                    continue;
                }
                Event::Idle => break,
                Event::Stdin => {
                    stdin_pending = false;
                    continue;
                }
                Event::Stdout(result) => (
                    stdout.output(
                        result.map_err(spawn_error)?,
                        &mut scratch,
                        callback.is_some(),
                    ),
                    ShellOutputStream::Stdout,
                ),
                Event::Stderr(result) => (
                    stderr.output(
                        result.map_err(spawn_error)?,
                        &mut scratch,
                        callback.is_some(),
                    ),
                    ShellOutputStream::Stderr,
                ),
            };
            if raw.is_empty() {
                continue;
            }
            if let Err(error) = emit(callback.as_deref_mut(), &scratch, stream, context) {
                callback_error = Some(error);
                callback = None;
                active.terminate(&mut child).await;
            }
            if spill_error.is_none()
                && let Some(file) = &mut spill
                && let Err(error) = file.feed(raw, self, context).await
            {
                spill_error = Some(error);
                active.terminate(&mut child).await;
            }
            if status.is_some() {
                idle_at = Some(Instant::now() + STDIO_GRACE);
            }
        }
        if spill_error.is_none()
            && let Some(spill) = &mut spill
        {
            spill_error = spill.finish().await.err();
        }
        if callback_error.is_none() {
            for (decoder, stream) in [
                (&mut stdout.decoder, ShellOutputStream::Stdout),
                (&mut stderr.decoder, ShellOutputStream::Stderr),
            ] {
                if callback.is_none() {
                    break;
                }
                scratch.clear();
                decoder.decode_into(None, &mut scratch);
                if let Err(error) = emit(callback.as_deref_mut(), &scratch, stream, context) {
                    callback_error = Some(error);
                    active.terminate(&mut child).await;
                    break;
                }
            }
        }
        let spill_path = spill.and_then(Spill::into_path);
        if let Some(error) = callback_error {
            self.discard_spill(spill_path.as_deref(), context).await;
            return Err(error);
        }
        if expired || timeout_at.is_some_and(|at| at <= Instant::now()) || context.is_aborted() {
            active.terminate(&mut child).await;
            let mut error = if expired || timeout_at.is_some_and(|at| at <= Instant::now()) {
                timed_out(options.timeout)
            } else {
                aborted()
            };
            error.spill_path = spill_path;
            return Err(error);
        }
        if let Some(error) = spill_error {
            self.discard_spill(spill_path.as_deref(), context).await;
            return Err(error);
        }
        active.armed = false;
        let status = status.ok_or_else(|| {
            ExecutionError::new(ExecutionErrorCode::SpawnError, "Child process did not exit")
        })?;
        Ok(ShellExecResult {
            exit_code: exit_code(status),
            spill_path,
        })
    }

    async fn cleanup(&self, _context: &Context) {
        self.commands.cleanup();
    }
}

impl LocalExecutionEnv {
    async fn discard_spill(&self, path: Option<&str>, context: &Context) {
        if let Some(dir) = path
            .and_then(|path| Path::new(path).parent())
            .and_then(Path::to_str)
        {
            let _ = self
                .remove(
                    dir,
                    Some(RemoveOptions {
                        recursive: true,
                        force: true,
                    }),
                    &context.without_abort_signal(),
                )
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    fn has_win_path(existing: &[&str], candidate: &Path) -> bool {
        let normalized = normalize_win32_path(&candidate.to_string_lossy());
        existing
            .iter()
            .any(|path| normalize_win32_path(path).eq_ignore_ascii_case(&normalized))
    }

    #[tokio::test]
    async fn windows_shell_prefers_git_bash_of_git_on_path_over_wsl_launcher() {
        let existing = [
            r"C:\Scoop\apps\git\current\bin\bash.exe",
            r"C:\Windows\System32\bash.exe",
        ];
        let resolved = resolve_windows_shell_with(
            |key| match key {
                "ProgramFiles" => Some(OsString::from(r"C:\Program Files")),
                "ProgramFiles(x86)" => Some(OsString::from(r"C:\Program Files (x86)")),
                _ => None,
            },
            |path| async move { has_win_path(&existing, &path) },
            |exe| async move {
                match exe {
                    "git.exe" => Some(PathBuf::from(r"C:\Scoop\shims\git.exe")),
                    "bash.exe" => Some(PathBuf::from(r"C:\Windows\System32\bash.exe")),
                    _ => None,
                }
            },
            |_git| async move { Some("C:/Scoop/apps/git/current/usr/bin/SH.EXE\r\n".into()) },
        )
        .await
        .expect("git bash resolves");
        assert_eq!(
            normalize_win32_path(&resolved.to_string_lossy()),
            r"C:\Scoop\apps\git\current\bin\bash.exe"
        );
    }

    #[tokio::test]
    async fn windows_shell_uses_program_files_without_spawning_git() {
        let existing = [r"C:\Program Files\Git\bin\bash.exe"];
        let probed_path = Arc::new(AtomicBool::new(false));
        let probed_git = Arc::new(AtomicBool::new(false));
        let resolved = resolve_windows_shell_with(
            |key| match key {
                "ProgramFiles" => Some(OsString::from(r"C:\Program Files")),
                _ => None,
            },
            |path| async move { has_win_path(&existing, &path) },
            |_exe| {
                let probed_path = probed_path.clone();
                async move {
                    probed_path.store(true, Ordering::SeqCst);
                    None
                }
            },
            |_git| {
                let probed_git = probed_git.clone();
                async move {
                    probed_git.store(true, Ordering::SeqCst);
                    None
                }
            },
        )
        .await
        .expect("program files bash resolves");
        assert_eq!(
            normalize_win32_path(&resolved.to_string_lossy()),
            r"C:\Program Files\Git\bin\bash.exe"
        );
        assert!(!probed_path.load(Ordering::SeqCst));
        assert!(!probed_git.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn windows_shell_falls_back_to_bash_on_path_when_git_var_fails_or_is_nonstandard() {
        let existing = [
            r"C:\Custom\Git\bin\bash.exe",
            r"C:\Windows\System32\bash.exe",
        ];
        // Old Git (<2.47): git var returns None.
        let old_git = resolve_windows_shell_with(
            |_| None,
            |path| async move { has_win_path(&existing, &path) },
            |exe| async move {
                match exe {
                    "git.exe" => Some(PathBuf::from(r"C:\Custom\Git\cmd\git.exe")),
                    "bash.exe" => Some(PathBuf::from(r"C:\Windows\System32\bash.exe")),
                    _ => None,
                }
            },
            |_| async move { None },
        )
        .await
        .expect("fallback bash");
        assert_eq!(old_git, PathBuf::from(r"C:\Windows\System32\bash.exe"));

        // Unexpected shell path outside usr\bin\sh.exe layout.
        for bad_shell in ["C:/msys64/usr/bin/bash.exe\n", "/bin/sh\n"] {
            let fallback = resolve_windows_shell_with(
                |_| None,
                |path| async move { has_win_path(&existing, &path) },
                |exe| async move {
                    match exe {
                        "git.exe" => Some(PathBuf::from(r"C:\Custom\Git\cmd\git.exe")),
                        "bash.exe" => Some(PathBuf::from(r"C:\Windows\System32\bash.exe")),
                        _ => None,
                    }
                },
                |_| async move { Some(bad_shell.to_owned()) },
            )
            .await
            .expect("fallback bash");
            assert_eq!(fallback, PathBuf::from(r"C:\Windows\System32\bash.exe"));
        }

        // Valid layout, but `<root>\bin\bash.exe` does not exist on disk.
        let missing_launcher = resolve_windows_shell_with(
            |_| None,
            |path| async move { has_win_path(&[r"C:\Windows\System32\bash.exe"], &path) },
            |exe| async move {
                match exe {
                    "git.exe" => Some(PathBuf::from(r"C:\Missing\Git\cmd\git.exe")),
                    "bash.exe" => Some(PathBuf::from(r"C:\Windows\System32\bash.exe")),
                    _ => None,
                }
            },
            |_| async move { Some("C:/Missing/Git/usr/bin/sh.exe\n".into()) },
        )
        .await
        .expect("fallback bash");
        assert_eq!(
            missing_launcher,
            PathBuf::from(r"C:\Windows\System32\bash.exe")
        );
    }

    #[tokio::test]
    async fn windows_shell_unavailable_error_documents_git_on_path_probe() {
        let error = resolve_windows_shell_with(
            |key| (key == "ProgramFiles").then(|| OsString::from(r"C:\Program Files")),
            |_| async move { false },
            |_| async move { None },
            |_| async move { None },
        )
        .await
        .expect_err("no shell found");
        assert_eq!(error.code, ExecutionErrorCode::ShellUnavailable);
        assert!(
            error
                .message
                .contains("the Git for Windows installation of git.exe on PATH"),
            "{}",
            error.message
        );
    }
}
