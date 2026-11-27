# pi-mac

Rust companion for p2. The portable library owns process supervision, login-shell environment capture, server discovery, Unix dev control, backups, owner sign-in, Radius OAuth and grants, brain ordering, diagnostics and signed appcasts. The macOS adapter keeps AppKit and WebKit on the main thread and sends only Rust data to background workers.

## Run

Note: `pi-mac` is compile-checked on macOS in CI, while live macOS AppKit/`WKWebView` verification is currently deferred (see [ADR-005](../../rust/DECISIONS.md)). The app runs the native Rust `p2` server: the one it carries in `Contents/Resources/bin/p2`, else `p2` on the login shell's `PATH`.

On Linux, you can exercise the portable executable against a native `p2` binary:

```sh
cargo build --locked -p pi-rs-p2-mac --bin pi-mac
P2MAC_BINARY=/path/to/p2 P2MAC_DATA_DIR=/path/to/private-test-data \
  P2MAC_PORT=8798 P2MAC_FAKE=1 target/debug/pi-mac --serve
P2MAC_ATTACH=8798 P2MAC_DATA_DIR=/path/to/private-test-data target/debug/pi-mac --diag
```

`--check-config` prints effective settings without starting p2. `--diag` probes the selected server and, when `P2MAC_DIAG_DIR` and `P2MAC_DIAG_BRIDGE` are set, records the portable bridge. `--serve` watches or runs p2, stopping only a server it owns when interrupted. In its current form, `--serve` runs the native `p2` binary with `PORT`, `P2_DATA_DIR` and, in dev mode, `--dev`; the standalone Rust `p2` server lives in `crates/pi-rs-p2`.

For a macOS development bundle, run [`scripts/package-p2-mac.sh`](../../scripts/package-p2-mac.sh): it builds the app and the p2 server and writes `build/Pi.app` with the server in `Contents/Resources/bin/p2`. Launch the bundle without arguments. It uses the `com.earendil.pi` defaults domain. Set `SUFeedURL` and `SUPublicEDKey` in signed builds to enable update checks. Manual checks verify the Sparkle enclosure's exact length and Ed25519 signature before unpacking the update; the updater then replaces the app bundle in place, so the next launch runs the new bundle.

## Settings

| Variable | Default or effect |
|---|---|
| `P2MAC_BINARY` | The native `p2` binary to run; otherwise the bundled one, else `p2` on the login-shell PATH. |
| `P2MAC_PORT`, `P2MAC_DATA_DIR` | `8788`, `~/.pi/p2`. |
| `P2MAC_MODE`, `P2MAC_FAKE` | `start` or `dev`; `1` enables the faux provider. |
| `P2MAC_ATTACH` | Use an external loopback port; `P2MAC_DATA_DIR` then names that server's data. |
| `PI_RADIUS_GATEWAY`, `P2MAC_SIGN_IN`, `P2MAC_BRAINS` | New sign-ins use `https://radius.pi.dev`; `headless` or a brain-list fixture is for development. |
| `P2MAC_DIAG_DIR`, `P2MAC_DIAG_BRIDGE`, `P2MAC_DIAG_OPEN`, `P2MAC_DIAG_PATH` | Write numbered diagnostics; optionally drive a theme probe or open selected brains. |

The macOS UI stores settings in UserDefaults. CLI diagnostics use `$XDG_CONFIG_HOME/pi-mac/settings.json` (or `~/.config/pi-mac/settings.json`). The app and p2 share the selected server's `radius.json`. Managed server output is appended to `~/Library/Logs/Pi/server-<port>.log`.

On macOS, diagnostics record WebKit page and chrome state, theme replies and refusals, menu and button behavior, event timing, and page/window snapshots. They do not sample per-frame screen captures during fades.

## Verify

```sh
cargo fmt --all -- --check
cargo clippy --offline --locked -p pi-rs-p2-mac --all-targets -- -D warnings
cargo test --offline --locked -p pi-rs-p2-mac
```

The Linux tests require cached Cargo dependencies and Node for the child-process fixture (Linux listener discovery reads `/proc/net/tcp` and `/proc/<pid>/fd` directly). They use temporary directories, local sockets, and loopback HTTP servers, not an external service. macOS AppKit/WKWebView linking, bundle signing, and live UI testing require a macOS machine and are not part of the Linux gate.

Linux x86_64: fmt and Clippy passed; all 16 offline tests passed (3 network, 9 portable, 4 supervisor).
The AppKit/WebKit shell also passed `cargo check --target aarch64-apple-darwin -j 2` from Linux using the Apple target standard library.

The real-checkout Linux smoke started upstream p2 under Rust supervision, checked configuration and attached diagnostics, completed the local owner challenge/claim, rejected an unrelated owner key, and shut down on SIGTERM.
