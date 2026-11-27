# Chord guest rollout fixtures

These are actual Component Model binaries compiled from `guest.rs` against the crate's `wit/plugin.wit`. The tests need no guest compiler, WASI adapter, or network access.

| Component | Guest behavior |
| --- | --- |
| `v1.wasm` | Adds the argument to a counter in guest memory and publishes the new value to Chord. |
| `reject.wasm` | Returns a guest error from `setup`, so the host keeps its previous instance. |
| `double.wasm` | Doubles the argument before adding it, proving the new guest instance is serving. |

To regenerate, make a scratch Cargo package named `chord-b3-fixture` (edition 2024). Copy `guest.rs` to `src/bin/guest.rs`, the crate's `wit/plugin.wit` to `wit/plugin.wit`, and `encode.rs` to `src/bin/encode.rs`. Use these target-specific dependencies:

```toml
[target.'cfg(target_arch = "wasm32")'.dependencies]
wit-bindgen = "=0.57.1"

[target.'cfg(not(target_arch = "wasm32"))'.dependencies]
wit-component = "=0.261.0"
```

With Rust 1.98.0 and the `wasm32-unknown-unknown` target installed, run from the scratch package:

```sh
cargo build --offline --bin encode
for revision in v1 reject double; do
    B3_REVISION="$revision" CARGO_PROFILE_RELEASE_STRIP=debuginfo \
        cargo build --offline --release --bin guest --target wasm32-unknown-unknown || exit
    target/debug/encode target/wasm32-unknown-unknown/release/guest.wasm "$revision.wasm" || exit
done
```

Copy the three generated components to this directory and run `--test b3_guest`.

`facet.wasm` exercises the extended manifest in `facet.json`. Its Rust source
is `facet.rs`. The component declares a parameterized object with a document,
job, store callback, event and keyed member, a remote type, an HTTP route and a
registry contribution.

To regenerate, create a scratch Rust binary package named `chord-facet-fixture`
(edition 2024) with dependencies `wit-bindgen = "=0.57.1"` and `serde_json = "1"`.
Copy `facet.rs` to `src/main.rs`, `facet.json` to `src/facet.json`, and the
crate's `wit/plugin.wit` to `wit/plugin.wit`. Then run:

```sh
RUSTFLAGS="--remap-path-prefix=$HOME=/home/user" CARGO_PROFILE_RELEASE_STRIP=symbols \
    cargo build --offline --release --target wasm32-unknown-unknown
wasm-tools component new target/wasm32-unknown-unknown/release/chord-facet-fixture.wasm -o facet.wasm
```
