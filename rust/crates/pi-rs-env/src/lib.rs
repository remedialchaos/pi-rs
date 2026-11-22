//! Remote execution environment client and daemon library (Milestone 2.3a).
//!
//! The `daemon` module is the integrated Rust `pi-env` daemon (upstream `packages/env/daemon`). The other modules are
//! the Rust port of the TypeScript client transport: binary framing, connection lifecycle, SSH bootstrap, and the
//! process, terminal and watcher handles. The durable-facing `ExecutionEnv` adapters follow in 2.3b.

mod sha256;

pub mod connection;
pub mod daemon;
pub mod errors;
pub mod framing;
pub mod process;
pub mod ssh;
pub mod terminal;
pub mod watch;

pub use connection::{
    AbortController, AbortSignal, CommandSource, Connection, ConnectionOptions, RemoteError,
    RemoteInfo, Reply, RequestOptions, is_connection_lost, node_platform,
};
pub use errors::{FileError, FileErrorCode, file_error_code, to_file_error};
pub use framing::{EVENT, MAX_FRAME, MAX_PAYLOAD, PING, REQUEST, RESULT};
pub use process::{ProcessOptions, RemoteProcess, open_process};
pub use ssh::{
    HostKeyChangedError, HostKeyUnknownError, RemoteArch, RemotePlatform, RemotePlatformKind,
    RemoteShell, SshConnectOptions, SshError, SshFailure, SshTarget, connect_ssh, deploy_daemon,
    detect_platform, forget_host_key, local_daemon, packaged_daemon, scan_host_key, ssh_arguments,
    ssh_connection,
};
pub use terminal::{RemoteTerminal, TerminalForeground, TerminalOptions, open_terminal};
pub use watch::{
    FileChange, RemoteWatchOptions, RemoteWatcher, WatchExclude, WatchMode, WatchTarget,
};
