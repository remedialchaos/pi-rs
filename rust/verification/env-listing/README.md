# Daemon-backed listing proof

Use Linux, rust-anything 3.0.0, the workspace Rust toolchain, and Node with native TypeScript support. Start in the checkout root. Install the upstream npm dependencies with `npm install --ignore-scripts`, or set `PI_RS_NODE_ROOT` to a checkout with those dependencies already installed.

### Set up a fresh proof

```bash
root=$(pwd -P)
v="$root/rust/verification/env-listing"
p="$root/rust/pi-rs-env-listing-proof"
modules=${PI_RS_NODE_ROOT:-$root}
loader="--node-arg=--import=$root/apps/p2/src/source-resolver.ts"
export TMPDIR=${TMPDIR:-$(mktemp -d /var/tmp/pi-rs-tmp-env-listing.XXXXXXXX)}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$root/rust/crates/target}
mkdir -p "$TMPDIR" "$CARGO_TARGET_DIR"
export CARGO_TARGET_DIR=$(realpath "$CARGO_TARGET_DIR")
export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_PROFILE_RELEASE_DEBUG=0
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-1}
export RUST_ANYTHING_COMMAND_TIMEOUT=${RUST_ANYTHING_COMMAND_TIMEOUT:-7200}
export CARGO_PROFILE_RELEASE_OPT_LEVEL=0 CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=256
export LISTING_FIXTURE="$v/fixture"
if test -z "${PI_ENV_DAEMON:-}"; then
    cargo build --locked --manifest-path "$root/rust/crates/Cargo.toml" -p pi-rs-env --bin pi-env
    export PI_ENV_DAEMON="$CARGO_TARGET_DIR/debug/pi-env"
fi
test -x "$PI_ENV_DAEMON"
```

Extract the temporary oracle from the recipe. Leave it in place through the gate so the tool can verify the original source fingerprint. Use a fresh ignored proof directory for the following scaffold commands.

```bash
awk '/^```typescript$/ { body=1; next } /^```$/ { body=0 } body' "$v/oracle.md" > "$v/listing.ts"
rust-anything bridge "$v" listing.ts --kind napi --name pi-rs-env-listing-proof --out "$p"
sed -i '/^\[dependencies\]/a pi-rs-env = { path = "../crates/pi-rs-env" }\npi-rs-chord = { path = "../crates/pi-rs-chord" }\npi-rs-durable = { path = "../crates/pi-rs-durable" }' "$p/Cargo.toml"
cp "$v/port.rs" "$p/src/port.rs"
ln -s "$CARGO_TARGET_DIR" "$p/target"
```

rust-anything 3.0.0 places its native Cargo output under the proof directory's `target`. The symlink directs it to the chosen cache.

### Record, compare and measure

```bash
rust-anything record "$p" --count 0 --inputs "$v/inputs.jsonl" --resolve-from "$modules" "$loader"
cmp "$p/cases.jsonl" "$v/cases.jsonl"
rust-anything build "$p"
rust-anything diff "$p" "$loader"
rust-anything bench "$p" --resolve-from "$modules" "$loader" --json
rust-anything gate "$p" "$loader"
rm "$v/listing.ts"
```

For Rust replay from the retained corpus, copy `"$v/cases.jsonl"` to `"$p/cases.jsonl"` after scaffolding and run the build and diff commands. Recording and benchmarking import the upstream TypeScript. The generated native module calls the Rust production adapter.

Each boolean in `inputs.jsonl` selects one production adapter. On one live connection it lists the fixture directory, lists it again with a one-entry limit, lists the regular file `/etc/hosts` as a directory, requests an unsafe integer limit, and calls with a pre-aborted context. Both clients use cwd `/`. The corpus preserves entry order, truncation, and the error code, message and path. The benchmark includes connection setup and teardown. Its timings apply to this workload and the unoptimized proof build.
