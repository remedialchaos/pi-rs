# p2 core milestone

The native `p2` binary starts a Chord server on loopback. It locks the data directory, checks
the storage layout, opens the private store and files, publishes its bound port, loads server
facets, and shuts down HTTP, Chord, and SQLite. The binary embeds the public installable-app
files and browser sign-in page. A packaged build also embeds the authenticated browser shell
and its WASM, module, stylesheet and fonts.

## Plugin and session host interfaces

- Register native Chord facets with `api::v1::PluginRegistry::register(name, FacetKind, facet)`.
  Each plugin name supports one facet per host kind. `select` validates requested names and
  returns them in deterministic order. `facets` returns the Chord plugin facets for a host.
- Pass the registry to `hosts::server::start_server` through `ServerOptions`. HTTP plugins can
  use `HttpContribution::facet()`. `P2Server::reload` returns Chord's per-plugin report.
  `hosts::client::start_client_host` loads web or TUI facets over a caller-supplied Chord link.
- `api::v1::server::SessionWorkers` accepts native process options and a catalog callback that
  supplies worker initialization values. Its explicit `hold` checks session address and access,
  shares a process across concurrent holds for the same session, and forwards the caller over
  the trusted worker link. It also exposes explicit retry and shutdown.
- `hosts::worker::start_session_worker` validates the Chord handshake and opens the private store
  and files for that session. `start_session_worker_with` accepts a runtime-provided
  `WorkerBoundary`. The version 2 `p2.worker-boundary` contract serves server-only prepare,
  cancel and relocate calls. The boundary fences new work, publishes committed locations and
  retains the fence if the move fails.
- `upstream::find_app`, `list_roots` and `SourceResolver` inspect upstream source without running it.
  The first root owns a plugin name. Resolved alias targets stay within the upstream checkout.

## Gates

| Check | Evidence |
| --- | --- |
| Native binary | Final offline build passed. The standalone binary ran from a disposable directory without `apps/p2`. Health, the embedded manifest and clean TERM shutdown passed the local smoke. |
| Formatting and Clippy | Package formatting and all-target Clippy with warnings denied passed. |
| Crate tests and timing | Four passed with zero removed. There were no tests in the inherited crate. The three-test suite before the worker follow-up reported 1.94 s of test execution and 26.82 s including compilation. The final four tests reported 2.18 s of execution and 18.66 s including compilation. These are run timings, with no performance comparison claimed. |
| HTTP parity | Coordinator recorded upstream responses and independently replayed all six on the standalone Rust binary. All six passed. Subsequent changes affect worker startup and session-host contracts. Sign-in template matches the captured TUI sign-in body after nonce substitution. The manifest body matches byte for byte. |
| Worker and session-host behavior | A real native child served its server-only Chord data hold. Two concurrent holds shared one start, an unauthorized hold was denied before spawn, and a mismatched process handshake was refused. A separate Chord link test verified the lifecycle variant, refusal, cancellation, fencing and location updates. |
| Mutation checks | Four tests retained. Each failed its intended mutation. The mutations changed health success, replaced plugin root priority, bypassed worker address or admission checks, and lifted the relocation fence early. The sources were restored. |
| Rust-anything leaf build, diff, benchmark and gate | Commands reported missing bridge metadata or leaf recordings for this native whole-app crate. The coordinator's app replay is the HTTP behavior gate. No worker oracle replay or performance benchmark has run. |

## Shared plugin contracts

The native `api::v1::p2_contracts::lookup` resolves 106 exported Chord
contracts. The parity fixture covers all 130 historical versions, including
variant and member order, schema and byte markers, admission fingerprints
and edit history. Plugin-private declarations belong to their plugins.

| Contract | Version | Wire fingerprint |
| --- | ---: | --- |
| `p2.access` | 1 | `2435548bc2ca4912` |
| `p2.sessions` | 7 | `1e273ec42b804553` |
| `p2.chat` | 6 | `15b5fd3b4b340eca` |
| `p2.catalog` | 5 | `77531524b1ada01d` |

Two contract tests were added and zero removed. Making the chat request ID
optional failed both tests. Restoring the upstream schema passed both in
0.54 seconds. Formatting and affected-crate Clippy passed. The existing
native worker test group timed out before and after this change in 3.93 and
5.73 seconds, respectively. Those runs did not complete the full crate suite.

## Limits

The standalone entry point starts with an empty native plugin registry. The session catalog
and Durable Harness backend still need production wiring. The port of the Durable Harness
is open as milestone 2.2b. Explicit session demand is available to native plugins. Automatic
routing from the HTTP Chord connection, catalog-coordinated pause and relocation, crash restart
policy and worker activity from Durable tasks still need integration. Owner claim, credential
and session flows require their plugins. Authenticated TUI bundle delivery, Radius framing,
telemetry and dev supervision remain open. HTTP responses outside
the six recorded cases, including errors before route dispatch, have limited parity evidence.

## Browser asset and production handoff

### Build a standalone p2 with the browser bundle

Run from the repository root with the Rust `wasm32-unknown-unknown` target, the
`wasm-bindgen` CLI version matching `Cargo.lock` (0.2.130), and npm dependencies
installed from `package-lock.json` with `npm ci --ignore-scripts`. esbuild and the
Elanor asset packages are build inputs.

```sh
export TMPDIR=/var/tmp/pi-rs-tmp-p2-assets
export CARGO_TARGET_DIR=/var/tmp/pi-rs-target-p2-assets
export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
assets="$PWD/rust/crates/target/p2-browser-assets"
mkdir -p "$TMPDIR" "$CARGO_TARGET_DIR" "$assets"
cargo build --manifest-path rust/crates/Cargo.toml -p pi-rs-p2-web \
  --target wasm32-unknown-unknown --offline --locked -j 1
wasm-bindgen --target web --no-typescript --out-name app --out-dir "$assets" \
  "$CARGO_TARGET_DIR/wasm32-unknown-unknown/debug/pi_rs_p2_web.wasm"
node_modules/.bin/esbuild rust/crates/pi-rs-p2-web/assets/browser.css \
  --bundle --loader:.woff2=file --outfile="$assets/p2.css"
P2_BROWSER_ASSETS="$assets" cargo build --manifest-path rust/crates/Cargo.toml \
  -p pi-rs-p2 --bin p2 --offline --locked -j 1

package="$PWD/rust/crates/target/p2-standalone"
vendor="$PWD/node_modules/@earendil-works"
mkdir -p "$package/licenses"
cp "$CARGO_TARGET_DIR/debug/p2" "$package/"
cp LICENSE "$package/LICENSE"
cp "$vendor/elanor-fonts/licenses/commit-mono.txt" "$package/licenses/"
cp "$vendor/elanor-fonts/licenses/mallos-serif.txt" "$package/licenses/"
cp "$vendor/design-tokens/LICENSE" "$package/licenses/design-tokens.txt"
cp "$vendor/elanor-react/LICENSE" "$package/licenses/elanor-react.txt"
```

Distribute `p2-standalone/` with its browser asset notices. The build script validates the asset set,
embeds the files in the binary and computes the SHA-256-based version. At runtime,
p2 serves the version at `/_chord/web/base` and the authenticated bundle at
`/_chord/web/<version>/`. CSS font URLs stay relative to the stylesheet. The
shell publishes the version as metadata. Rust/WASM reads it to set
`chordPage.base` and open `/chord?web=<version>`. Authenticated HTML and its CSP
share the core server's nonce. The policy allows same-origin assets and the
Chord socket. Authenticated pages and bundle files use `Cache-Control: no-store`.

Core builds with `P2_BROWSER_ASSETS` unset still serve sign-in and public
installable-app files. An authorized app page on that build returns HTTP 503.
The variable is a compile-time input. Start the packaged binary with the usual
`PORT` and `P2_DATA_DIR` settings.

The public base route reads `hosts::web::browser_base()`. That function also
exposes the packaged version to the server for the planned stale-page check.

| Ownership boundary | Files and remaining work |
| --- | --- |
| Browser bundle | `pi-rs-p2-web` owns the WASM client, JS loader, CSS, fonts and binary Chord client. The main commit's milestone records its loopback Chromium test against upstream p2. |
| Asset and page integration | `pi-rs-p2/src/hosts/web.rs` serves the authenticated shell to owners and to guests on root and their granted session pages. The app's code assets require an owner or guest principal. Chord resolves credentials and enforces origin and Fetch Metadata admission before dispatch. |
| Production server and plugin composition | `main.rs` owns the production registry selected at boot. `hosts/server.rs` owns authenticated HTTP and WebSocket admission and Chord routing. Register the providers for `p2.access` v1 and `p2.sessions` v7 on the server and preserve session identity through the WebSocket upgrade. The browser also expects `p2.chat` v6 on the session address. Match the upstream contract fingerprints and member indexes. |
| Production session worker | `hosts/session_workers.rs` owns on-demand child routing and caller propagation. Wire the session catalog callback and Chord route into it. `hosts/worker.rs` owns child boot and worker facets. Install Chat and the Durable Harness backend there, using `runtime/session_worker.rs` for lifecycle coordination. |
| Native client host | `hosts/client.rs` owns native web and TUI facet loading over a caller-supplied Chord link. The compiled browser connects directly. TUI delivery remains a separate integration. |

### Live asset verification

- Rebuilt the browser bundle from source using the existing lockfile-installed
  npm dependencies. A second wasm-bindgen and esbuild run produced identical
  asset bytes. Started the copied package outside the checkout with no runtime
  asset path. Health, the base version, installable manifest, anonymous sign-in
  and asset refusal passed. The process shut down cleanly on TERM.
- Chromium 157 used separate disposable owner and guest profiles against the
  native server with a credential fixture. The owner sign-in form rejected
  invalid and reused tokens. The guest received 403 for an ungranted session
  and 200 for its granted conversation. Both profiles mounted the app under
  production CSP, fetched the JS, WASM, CSS and five WOFF2 files from p2, and
  received a Chord upgrade and binary frame using the packaged base. The HTML
  contained the bootstrap and no identity or session data.
- The embedded `sw.js` matches upstream byte-for-byte. In a separate check
  after the owner run, the active worker returned cached WASM with HTTP 200
  after cookie removal. A direct server request returned 401. The worker also
  stored the metadata-only HTML shell. This is inherited offline behavior.
- The recorded HTTP replay passed 6/6 on the copied package. An earlier run
  received a transient 404 on `/__p2_dev`. That case passed in isolation and
  in the subsequent full replay.

### Browser asset gates

| Check | Evidence |
| --- | --- |
| Build input validation | The offline WASM and native builds passed. A bundle missing `app_bg.wasm` failed at compile time. |
| Owner and guest admission | Fresh Chromium profiles mounted under CSP using real HTTP responses. The guest's ungranted session returned 403. |
| Shared page nonce | The HTTP test compares two authenticated HTML responses from one server and matches each script nonce to its CSP. Replacing the server nonce with the Chord request nonce failed that assertion. |
| Mutation checks | Two tests added, zero removed. Removing the guest grant check and changing the WASM MIME type each failed the corresponding test. The original code was restored for the final suite. |
| Native tests | Before the change, p2 passed 4 tests in 1.17 s. After the change, p2 passed 6 tests in 4.12 s without the bundle and 6 tests in 3.17 s with it. p2-web passed 135 native tests. Times sum test harness execution and exclude build and lock waits. |
| Formatting and lint | Formatting passed for both crates. Offline all-target Clippy passed with warnings denied for core-only and packaged p2, native p2-web and the WASM target. |

For production acceptance, use the native server and worker with a disposable catalog and fake
model. Sign in, create a session, send a message, observe the user and assistant rows, and
reload to verify persistence. Exercise access rejection for a guest without a session grant.
The browser milestone's upstream test used injected assets and CSP bypass. The native
browser checks for this slice exercise asset delivery. Native chat and persistence still
require the production providers and worker wiring.

### Browser identity and stale pages

The HTTP host prefers the owner when multiple credentials match. Over Chord,
the browser can read its own access grant. An authenticated client presenting
an outdated browser bundle receives the reload close frame before Chord sends
its hello. The unchanged upstream device-authentication and stale-page cases
each passed. Changing the owner classification and accepting the stale link
each failed the corresponding Rust test.

Two tests were added and zero removed. The previous eight-test revision had a
worker-startup timeout under load, so a passing time for that run is unavailable.
The fixture now uses the production startup deadline. All ten tests passed in
23.22 s of test execution with production browser assets. Workspace formatting
and strict all-target Clippy for p2 and Chord passed.
