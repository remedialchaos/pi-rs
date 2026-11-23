# p2 core milestone

The native `p2` binary starts a Chord server on loopback. It locks the data directory, checks
the storage layout, opens the private store and files, publishes its bound port, loads server
facets, and shuts down HTTP, Chord, and SQLite. The binary embeds the public installable-app
files and browser sign-in page.

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

## Limits

The standalone entry point starts with an empty native plugin registry. The session catalog
and Durable Harness backend still need production wiring. The port of the Durable Harness
is open as milestone 2.2b. Explicit session demand is available to native plugins. Automatic
routing from the HTTP Chord connection, catalog-coordinated pause and relocation, crash restart
policy and worker activity from Durable tasks still need integration. Owner claim, credential
and session flows require their plugins. Browser WASM and authenticated TUI bundle delivery are
separate integration work. Telemetry and dev supervision remain open. HTTP responses outside
the six recorded cases, including errors before route dispatch, have limited parity evidence.

## Browser asset and production handoff

Browser client source is on local main at `f49b10824`. The browser owner supplies
`app.js`, `app_bg.wasm`, `p2.css` and the five WOFF2 fonts. The coordinator reported the live
test bundle at `rust/crates/target/p2-browser-assets` in the main checkout. Treat it as a
build input for live testing. Production packaging should supply the same files without a
runtime dependency on that checkout.

| Ownership boundary | Files and remaining work |
| --- | --- |
| Browser bundle | `pi-rs-p2-web` owns the WASM client, JS loader, CSS, fonts and binary Chord client. The main commit's milestone records its loopback Chromium test against upstream p2. |
| Asset and page integration | `pi-rs-p2/src/hosts/web.rs` owns HTTP delivery of the bundle and the authenticated app page. Today it serves public files and the sign-in fallback. Authenticated requests reaching the fallback get 404. Provide `#root`, load the stylesheet and JS module, initialize WASM once, and set the local `chordPage.base` used by `/chord?web=<base>`. Serve WASM as `application/wasm` and preserve the stylesheet's relative font URLs. Coordinate CSP and bundle configuration through `hosts/server.rs` and `main.rs`. |
| Production server and plugin composition | `main.rs` owns the production registry selected at boot. `hosts/server.rs` owns authenticated HTTP and WebSocket admission and Chord routing. Register the providers for `p2.access` v1 and `p2.sessions` v7 on the server and preserve session identity through the WebSocket upgrade. The browser also expects `p2.chat` v6 on the session address. Match the upstream contract fingerprints and member indexes. |
| Production session worker | `hosts/session_workers.rs` owns on-demand child routing and caller propagation. Wire the session catalog callback and Chord route into it. `hosts/worker.rs` owns child boot and worker facets. Install Chat and the Durable Harness backend there, using `runtime/session_worker.rs` for lifecycle coordination. |
| Native client host | `hosts/client.rs` owns native web and TUI facet loading over a caller-supplied Chord link. The compiled browser connects directly. TUI delivery remains a separate integration. |

For asset acceptance, fetch the HTML, JS, WASM, CSS and fonts from p2 itself. Confirm the
anonymous sign-in flow still works and load the authenticated page in Chromium with production
CSP enabled. Asset delivery can be checked before the production providers land.

For production acceptance, use the native server and worker with a disposable catalog and fake
model. Sign in, create a session, send a message, observe the user and assistant rows, and
reload to verify persistence. Exercise access rejection for a guest without a session grant.
The browser milestone's upstream test used injected assets and CSP bypass. The coordinator's
6/6 native HTTP replay covers the existing front door. Production browser delivery and native
chat require their own live result.
