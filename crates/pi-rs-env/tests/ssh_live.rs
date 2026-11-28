#![cfg(unix)]
//! Real end-to-end SSH test against an ephemeral unprivileged `sshd` server.

use pi_rs_chord::context::Context;
use pi_rs_durable::env::{
    FileKind, FileListEntry, FileListing, FileSystem, Shell, ShellCommand, ShellExecOptions,
    ShellOutputInfo, ShellOutputStream,
};
use pi_rs_env::ssh::{accept_host_key, connect_ssh};
use pi_rs_env::{
    RemoteArch, RemoteExecutionEnv, RemoteExecutionEnvOptions, RemotePlatformKind,
    RemoteWatchOptions, SshConnectOptions, SshFailure, SshTarget, deploy_daemon, detect_platform,
    forget_host_key, scan_host_key,
};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn find_sshd() -> PathBuf {
    for candidate in ["/usr/sbin/sshd", "/usr/local/sbin/sshd", "/sbin/sshd"] {
        let path = Path::new(candidate);
        if path.is_file() {
            return path.to_path_buf();
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join("sshd");
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    panic!("sshd binary not found in /usr/sbin, /usr/local/sbin, /sbin, or PATH");
}

fn current_user() -> String {
    let output = Command::new("id")
        .arg("-un")
        .output()
        .expect("failed to execute `id -un`");
    assert!(
        output.status.success(),
        "`id -un` failed with status {}",
        output.status
    );
    let user = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(!user.is_empty(), "`id -un` returned an empty username");
    user
}

fn generate_ed25519_key(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(path)
        .status()?;
    assert!(
        status.success(),
        "ssh-keygen failed for {} with {status}",
        path.display()
    );
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

struct EphemeralSshd {
    _root: TempDir,
    child: Child,
    port: u16,
    user: String,
    identity_file: PathBuf,
    known_hosts_file: PathBuf,
    host_pub_key: PathBuf,
    remote_home: PathBuf,
    workspace: PathBuf,
}

impl EphemeralSshd {
    fn start() -> Result<Self, Box<dyn std::error::Error>> {
        let sshd = find_sshd();
        let user = current_user();
        let root = TempDir::new()?;
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700))?;

        let remote_home = root.path().join("remote_home");
        fs::create_dir(&remote_home)?;
        fs::set_permissions(&remote_home, fs::Permissions::from_mode(0o700))?;

        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace)?;
        fs::set_permissions(&workspace, fs::Permissions::from_mode(0o700))?;

        let host_key = root.path().join("host_ed25519");
        generate_ed25519_key(&host_key)?;
        let host_pub_key = root.path().join("host_ed25519.pub");

        let identity_file = root.path().join("client_ed25519");
        generate_ed25519_key(&identity_file)?;
        let client_pub_key = root.path().join("client_ed25519.pub");

        let authorized_keys = root.path().join("authorized_keys");
        fs::copy(&client_pub_key, &authorized_keys)?;
        fs::set_permissions(&authorized_keys, fs::Permissions::from_mode(0o600))?;

        let known_hosts_file = root.path().join("known_hosts");

        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        drop(listener);

        let pid_file = root.path().join("sshd.pid");
        let sshd_config = root.path().join("sshd_config");
        let config_text = format!(
            "Port {port}\n\
             ListenAddress 127.0.0.1\n\
             HostKey \"{host_key}\"\n\
             AuthorizedKeysFile \"{authorized_keys}\"\n\
             PubkeyAuthentication yes\n\
             PasswordAuthentication no\n\
             KbdInteractiveAuthentication no\n\
             StrictModes no\n\
             UsePAM no\n\
             PidFile \"{pid_file}\"\n\
             Subsystem sftp internal-sftp\n\
             SetEnv \"HOME={remote_home}\"\n",
            host_key = host_key.display(),
            authorized_keys = authorized_keys.display(),
            pid_file = pid_file.display(),
            remote_home = remote_home.display(),
        );
        fs::write(&sshd_config, config_text)?;

        let log_path = root.path().join("sshd.log");
        let log_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&log_path)?;

        let mut child = Command::new(&sshd)
            .args(["-D", "-e", "-f"])
            .arg(&sshd_config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log_file))
            .spawn()?;

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait()? {
                let logs = fs::read_to_string(&log_path).unwrap_or_default();
                return Err(format!("sshd exited prematurely with {status}: {logs}").into());
            }
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let logs = fs::read_to_string(&log_path).unwrap_or_default();
                return Err(
                    format!("timed out waiting for sshd on 127.0.0.1:{port}: {logs}").into(),
                );
            }
            std::thread::sleep(Duration::from_millis(25));
        }

        Ok(Self {
            _root: root,
            child,
            port,
            user,
            identity_file,
            known_hosts_file,
            host_pub_key,
            remote_home,
            workspace,
        })
    }

    fn target(&self) -> SshTarget {
        SshTarget {
            host: "127.0.0.1".to_string(),
            user: Some(self.user.clone()),
            port: Some(self.port),
            identity_file: Some(self.identity_file.to_string_lossy().into_owned()),
            known_hosts_file: self.known_hosts_file.to_string_lossy().into_owned(),
            host_key_alias: format!("pi-env-live-test-{}", self.port),
            ssh: None,
            config_file: Some("/dev/null".to_string()),
        }
    }

    fn connect_options(&self, binary: PathBuf) -> SshConnectOptions {
        SshConnectOptions {
            host: "127.0.0.1".to_string(),
            user: Some(self.user.clone()),
            port: Some(self.port),
            identity_file: Some(self.identity_file.to_string_lossy().into_owned()),
            known_hosts_file: self.known_hosts_file.to_string_lossy().into_owned(),
            host_key_alias: format!("pi-env-live-test-{}", self.port),
            ssh: None,
            config_file: Some("/dev/null".to_string()),
            binary: Some(binary),
            login_shell: false,
            on_log: None,
        }
    }
}

impl Drop for EphemeralSshd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct FileCleanupGuard {
    path: PathBuf,
}

impl Drop for FileCleanupGuard {
    fn drop(&mut self) {
        if self.path.exists() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[tokio::test]
async fn live_sshd_bootstrap_deploy_and_remote_execution_env() -> TestResult {
    let sshd = EphemeralSshd::start()?;
    let target = sshd.target();

    // 1. Strict host-key checking rejects untrusted hosts before scan + accept.
    assert!(matches!(
        detect_platform(&target),
        Err(SshFailure::UnknownKey(_))
    ));

    // 2. Scan and accept the ephemeral Ed25519 host key.
    let scanned = scan_host_key(&target)?;
    assert!(!scanned.lines.is_empty());
    assert!(!scanned.fingerprints.is_empty());

    let host_pub = fs::read_to_string(&sshd.host_pub_key)?;
    let pub_key_body = host_pub
        .split_whitespace()
        .nth(1)
        .ok_or("missing public key body in host_ed25519.pub")?;
    let expected_prefix = format!("{} ssh-ed25519 {pub_key_body}", target.host_key_alias);
    assert!(
        scanned
            .lines
            .iter()
            .any(|line| line.starts_with(&expected_prefix)),
        "expected scanned key starting with {expected_prefix:?}, got {:?}",
        scanned.lines
    );
    assert!(
        scanned
            .fingerprints
            .iter()
            .any(|fp| fp.contains("ED25519") && fp.contains("SHA256:")),
        "expected ED25519 SHA256 fingerprint, got {:?}",
        scanned.fingerprints
    );

    accept_host_key(&target, &scanned.lines)?;
    assert!(sshd.known_hosts_file.is_file());

    // 3. Detect remote platform, arch, and isolated home over real SSH.
    let remote = detect_platform(&target)?;
    let expected_platform = match std::env::consts::OS {
        "macos" => RemotePlatformKind::Darwin,
        _ => RemotePlatformKind::Linux,
    };
    let expected_arch = match std::env::consts::ARCH {
        "aarch64" => RemoteArch::Arm64,
        _ => RemoteArch::X64,
    };
    assert_eq!(remote.platform, expected_platform);
    assert_eq!(remote.arch, expected_arch);
    assert_eq!(remote.home, sshd.remote_home.to_string_lossy());
    assert!(remote.shell.is_none());

    // 4. Deploy compiled `pi-env`, verify SHA-256 & mode, and confirm second deploy hits "present" fast-path.
    let binary = Path::new(env!("CARGO_BIN_EXE_pi-env"));
    let deployed = deploy_daemon(&target, &remote, Some(binary))?;
    let cleanup = FileCleanupGuard {
        path: PathBuf::from(&deployed),
    };
    let expected_dir = format!("{}/.pi/mobile/tools/pi-env-", remote.home);
    assert!(
        deployed.starts_with(&expected_dir),
        "unexpected deployed path: {deployed}"
    );
    let deployed_path = Path::new(&deployed);
    assert!(deployed_path.is_file());
    let metadata = fs::metadata(deployed_path)?;
    assert_eq!(metadata.mode() & 0o777, 0o700);
    assert_eq!(fs::read(deployed_path)?, fs::read(binary)?);

    let first_ino = metadata.ino();
    let tools_dir = sshd.remote_home.join(".pi/mobile/tools");
    let sentinel = tools_dir.join("pi-env-fastpath-sentinel");
    fs::write(&sentinel, b"keep")?;

    let deployed_second = deploy_daemon(&target, &remote, Some(binary))?;
    assert_eq!(deployed_second, deployed);
    assert_eq!(fs::metadata(deployed_path)?.ino(), first_ino);
    assert!(
        sentinel.exists(),
        "second deploy_daemon should return on 'present' fast-path without running cleanup"
    );
    fs::remove_file(&sentinel)?;

    // 5. Connect over live SSH and exercise RemoteExecutionEnv (FileSystem + Shell).
    let (conn, connected_remote) = connect_ssh(sshd.connect_options(binary.to_path_buf()))?;
    assert_eq!(connected_remote.home, remote.home);
    let conn = Arc::new(conn);
    let info = conn.info()?;
    assert_eq!(info.protocol, 1);
    assert_eq!(info.version, "1.6.1");
    assert!(info.has_feature("listFiles"));

    let env = RemoteExecutionEnv::new(RemoteExecutionEnvOptions {
        id: "pi-env:ssh-live".to_string(),
        cwd: sshd.workspace.to_string_lossy().into_owned(),
        connection: Arc::clone(&conn),
        shell_path: None,
        shell_env: BTreeMap::from([("PI_SSH_LIVE".to_string(), "ok".to_string())]),
        watch: RemoteWatchOptions::default(),
    });
    let ctx = Context::background();
    env.write_file("nested/hello.txt", b"hello over ssh\nsecond line\n", &ctx)
        .await?;
    env.write_file("nested/other.txt", b"other", &ctx).await?;
    assert_eq!(
        env.read_text_file("nested/hello.txt", &ctx).await?,
        "hello over ssh\nsecond line\n"
    );
    assert_eq!(
        env.list_dir("nested", &ctx)
            .await?
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["hello.txt", "other.txt"]
    );
    assert_eq!(
        env.list_files(".", None, &ctx).await?,
        FileListing {
            entries: vec![
                FileListEntry {
                    kind: FileKind::Directory,
                    path: "nested".to_string(),
                },
                FileListEntry {
                    kind: FileKind::File,
                    path: "nested/hello.txt".to_string(),
                },
                FileListEntry {
                    kind: FileKind::File,
                    path: "nested/other.txt".to_string(),
                },
            ],
            truncated: false,
        }
    );

    let mut out = String::new();
    let mut err = String::new();
    let mut on_output = |text: &str, _: &Context, info: ShellOutputInfo| {
        match info.stream {
            ShellOutputStream::Stderr => err.push_str(text),
            ShellOutputStream::Stdout => out.push_str(text),
        }
        Ok(())
    };
    let exec_result = env
        .exec(
            ShellCommand::Script(
                "printf 'ssh:%s:%s' \"$PI_SSH_LIVE\" \"$(head -n 1 nested/hello.txt)\"; printf 'err-stream' >&2",
            ),
            Some(ShellExecOptions {
                on_output: Some(&mut on_output),
                ..Default::default()
            }),
            &ctx,
        )
        .await?;
    assert_eq!(exec_result.exit_code, 0);
    assert_eq!(out, "ssh:ok:hello over ssh");
    assert_eq!(err, "err-stream");

    drop(env);
    conn.close();
    assert_eq!(conn.info().unwrap_err().message, "Connection closed");

    // 6. Forgetting the host key restores strict UnknownKey rejection and cleanup removes the binary.
    forget_host_key(&target)?;
    assert!(matches!(
        detect_platform(&target),
        Err(SshFailure::UnknownKey(_))
    ));
    drop(cleanup);
    assert!(!deployed_path.exists());

    Ok(())
}
