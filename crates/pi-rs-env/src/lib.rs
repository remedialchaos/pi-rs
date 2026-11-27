//! Remote execution environment client and daemon library (Milestone 2.3a).
//!
//! The `daemon` module is the integrated Rust `pi-env` daemon (upstream `packages/env/daemon`). The other modules are
//! the Rust port of the TypeScript client transport: binary framing, connection lifecycle, SSH bootstrap, and the
//! process, terminal and watcher handles. The remote module exposes the durable filesystem and shell adapters.

#[cfg(feature = "client")]
mod sha256;

#[cfg(feature = "client")]
pub mod connection;
pub mod daemon;
#[cfg(feature = "client")]
pub mod errors;
#[cfg(feature = "client")]
pub mod framing;
#[cfg(feature = "client")]
pub mod process;
#[cfg(feature = "client")]
pub mod remote;
#[cfg(feature = "client")]
pub mod ssh;
#[cfg(feature = "client")]
pub mod terminal;
#[cfg(feature = "client")]
pub mod watch;

#[cfg(feature = "client")]
pub use connection::{
    AbortController, AbortSignal, CommandSource, Connection, ConnectionOptions, RemoteError,
    RemoteInfo, Reply, RequestOptions, is_connection_lost, node_platform,
};
#[cfg(feature = "client")]
pub use errors::{FileError, FileErrorCode, file_error_code, to_file_error};
#[cfg(feature = "client")]
pub use framing::{EVENT, MAX_FRAME, MAX_PAYLOAD, PING, REQUEST, RESULT};
#[cfg(feature = "client")]
pub use process::{ProcessOptions, RemoteProcess, open_process};
#[cfg(feature = "client")]
pub use ssh::{
    HostKeyChangedError, HostKeyUnknownError, RemoteArch, RemotePlatform, RemotePlatformKind,
    RemoteShell, SshConnectOptions, SshError, SshFailure, SshTarget, connect_ssh, deploy_daemon,
    detect_platform, forget_host_key, local_daemon, packaged_daemon, packaged_daemon_in,
    scan_host_key, ssh_arguments, ssh_connection,
};
#[cfg(feature = "client")]
pub use terminal::{RemoteTerminal, TerminalForeground, TerminalOptions, open_terminal};
#[cfg(feature = "client")]
pub use watch::{
    FileChange, RemoteWatchOptions, RemoteWatcher, WatchExclude, WatchMode, WatchTarget,
};

#[cfg(feature = "client")]
pub use remote::{
    LocalExecutionEnv, RemoteExecutionEnv, RemoteExecutionEnvOptions, list_files_over,
};
