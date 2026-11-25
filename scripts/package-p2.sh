#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/package-p2.sh [OPTIONS]

Build the pure-Rust p2 browser assets (CSS, WOFF2 fonts, and Leptos WASM bundle)
and optionally build the packaged `p2` binary with those assets embedded.

Options:
  --out-dir <DIR>    Directory for generated browser assets
                     (default: <target-dir>/p2-browser-assets)
  --release          Build WASM and p2 using the release profile (default: debug)
  --assets-only      Generate and verify browser assets only; skip building p2
  --target <TRIPLE>  Target triple passed to `cargo build -p pi-rs-p2 --bin p2`
  --locked           Forward --locked to all Cargo commands
  --offline          Forward --offline to all Cargo commands
  -h, --help         Show this help message and exit
EOF
}

die() {
    echo "package-p2.sh: error: $*" >&2
    exit 1
}

to_host_path() {
    local path="$1"
    case "$(uname -s)" in
        MINGW*|MSYS*|CYGWIN*)
            if command -v cygpath >/dev/null 2>&1; then
                cygpath -m "$path"
                return
            fi
            ;;
    esac
    printf '%s\n' "$path"
}

OUT_DIR=""
RELEASE=0
ASSETS_ONLY=0
HOST_TARGET=""
LOCKED=0
OFFLINE=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --out-dir)
            [[ $# -ge 2 && -n "${2:-}" ]] || { echo "package-p2.sh: error: --out-dir requires a directory argument" >&2; exit 2; }
            OUT_DIR="$2"
            shift 2
            ;;
        --out-dir=*)
            OUT_DIR="${1#*=}"
            [[ -n "$OUT_DIR" ]] || { echo "package-p2.sh: error: --out-dir requires a non-empty value" >&2; exit 2; }
            shift
            ;;
        --release)
            RELEASE=1
            shift
            ;;
        --assets-only)
            ASSETS_ONLY=1
            shift
            ;;
        --target)
            [[ $# -ge 2 && -n "${2:-}" ]] || { echo "package-p2.sh: error: --target requires a target triple" >&2; exit 2; }
            HOST_TARGET="$2"
            shift 2
            ;;
        --target=*)
            HOST_TARGET="${1#*=}"
            [[ -n "$HOST_TARGET" ]] || { echo "package-p2.sh: error: --target requires a non-empty value" >&2; exit 2; }
            shift
            ;;
        --locked)
            LOCKED=1
            shift
            ;;
        --offline)
            OFFLINE=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "package-p2.sh: error: unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"

if [[ -f "$REPO_ROOT/Cargo.toml" ]] && grep -Eq '^[[:space:]]*\[workspace\]' "$REPO_ROOT/Cargo.toml"; then
    WORKSPACE_DIR="$REPO_ROOT"
    MANIFEST="$REPO_ROOT/Cargo.toml"
elif [[ -f "$REPO_ROOT/rust/crates/Cargo.toml" ]]; then
    WORKSPACE_DIR="$REPO_ROOT/rust/crates"
    MANIFEST="$REPO_ROOT/rust/crates/Cargo.toml"
else
    die "could not locate workspace Cargo.toml at repo root or rust/crates/"
fi

LOCKFILE="$WORKSPACE_DIR/Cargo.lock"
[[ -f "$LOCKFILE" ]] || die "missing Cargo.lock at $LOCKFILE"

PINNED_WBG_VERSION="$(awk '
    { sub(/\r$/, "") }
    /^\[\[package\]\]$/ { in_pkg = 1; matched = 0; next }
    in_pkg && /^name = "wasm-bindgen"$/ { matched = 1; next }
    in_pkg && matched && /^version = "/ {
        v = $0
        sub(/^version = "/, "", v)
        sub(/"$/, "", v)
        print v
        exit
    }
    /^$/ { in_pkg = 0; matched = 0 }
' "$LOCKFILE")"

[[ -n "$PINNED_WBG_VERSION" ]] || die "could not determine pinned wasm-bindgen version from $LOCKFILE"

if ! command -v wasm-bindgen >/dev/null 2>&1; then
    die "wasm-bindgen CLI is not installed (expected $PINNED_WBG_VERSION). Install it with:
  cargo install wasm-bindgen-cli --version $PINNED_WBG_VERSION --locked"
fi

INSTALLED_WBG_VERSION="$(wasm-bindgen --version | awk '{print $2}' | tr -d '\r')"
if [[ "$INSTALLED_WBG_VERSION" != "$PINNED_WBG_VERSION" ]]; then
    die "wasm-bindgen version mismatch: found '$INSTALLED_WBG_VERSION', expected '$PINNED_WBG_VERSION' (pinned in $LOCKFILE). Install matching CLI with:
  cargo install wasm-bindgen-cli --version $PINNED_WBG_VERSION --locked"
fi

TARGET_DIR="${CARGO_TARGET_DIR:-$WORKSPACE_DIR/target}"
TARGET_DIR="${TARGET_DIR%/}"
ASSETS_DIR="${OUT_DIR:-$TARGET_DIR/p2-browser-assets}"
ASSETS_DIR="${ASSETS_DIR%/}"

if [[ "$RELEASE" -eq 1 ]]; then
    PROFILE_DIR="release"
else
    PROFILE_DIR="debug"
fi

WASM_PATH="$TARGET_DIR/wasm32-unknown-unknown/$PROFILE_DIR/pi_rs_p2_web.wasm"

EXE_EXT=""
if [[ -n "$HOST_TARGET" ]]; then
    case "$HOST_TARGET" in
        *windows*) EXE_EXT=".exe" ;;
    esac
else
    case "$(uname -s)" in
        MINGW*|MSYS*|CYGWIN*) EXE_EXT=".exe" ;;
    esac
fi

if [[ -n "$HOST_TARGET" ]]; then
    P2_BIN="$TARGET_DIR/$HOST_TARGET/$PROFILE_DIR/p2$EXE_EXT"
else
    P2_BIN="$TARGET_DIR/$PROFILE_DIR/p2$EXE_EXT"
fi

COMMON_CARGO_FLAGS=()
if [[ "$LOCKED" -eq 1 ]]; then
    COMMON_CARGO_FLAGS+=(--locked)
fi
if [[ "$OFFLINE" -eq 1 ]]; then
    COMMON_CARGO_FLAGS+=(--offline)
fi

PROFILE_FLAGS=()
if [[ "$RELEASE" -eq 1 ]]; then
    PROFILE_FLAGS+=(--release)
fi

TARGET_FLAGS=()
if [[ -n "$HOST_TARGET" ]]; then
    TARGET_FLAGS+=(--target "$HOST_TARGET")
fi

mkdir -p "$ASSETS_DIR"

MANIFEST_HOST="$(to_host_path "$MANIFEST")"
ASSETS_DIR_HOST="$(to_host_path "$ASSETS_DIR")"
WASM_PATH_HOST="$(to_host_path "$WASM_PATH")"

echo "Bundling p2 CSS and fonts into $ASSETS_DIR..." >&2
cargo run \
    --manifest-path "$MANIFEST_HOST" \
    "${COMMON_CARGO_FLAGS[@]}" \
    -p pi-rs-p2-web \
    --bin p2-bundle-assets \
    -- --out-dir "$ASSETS_DIR_HOST"

echo "Building pi-rs-p2-web for wasm32-unknown-unknown ($PROFILE_DIR)..." >&2
cargo build \
    --manifest-path "$MANIFEST_HOST" \
    "${COMMON_CARGO_FLAGS[@]}" \
    "${PROFILE_FLAGS[@]}" \
    -p pi-rs-p2-web \
    --target wasm32-unknown-unknown

[[ -s "$WASM_PATH" ]] || die "expected WASM artifact missing or empty: $WASM_PATH"

echo "Running wasm-bindgen ($INSTALLED_WBG_VERSION)..." >&2
wasm-bindgen \
    --target web \
    --no-typescript \
    --out-name app \
    --out-dir "$ASSETS_DIR_HOST" \
    "$WASM_PATH_HOST"

EXPECTED_ASSETS=(
    "app.js"
    "app_bg.wasm"
    "p2.css"
    "CommitMono-300-Regular.woff2"
    "CommitMono-400-Regular.woff2"
    "CommitMono-700-Regular.woff2"
    "MallosSerif-Italic-Variable.woff2"
    "MallosSerif-Variable.woff2"
)

for asset in "${EXPECTED_ASSETS[@]}"; do
    asset_path="$ASSETS_DIR/$asset"
    [[ -s "$asset_path" ]] || die "expected browser asset missing or empty: $asset_path"
done

if [[ "$ASSETS_ONLY" -eq 1 ]]; then
    printf '%s\n' "$ASSETS_DIR"
    exit 0
fi

echo "Building packaged p2 binary ($PROFILE_DIR)..." >&2
P2_BROWSER_ASSETS="$ASSETS_DIR_HOST" cargo build \
    --manifest-path "$MANIFEST_HOST" \
    "${COMMON_CARGO_FLAGS[@]}" \
    "${PROFILE_FLAGS[@]}" \
    "${TARGET_FLAGS[@]}" \
    -p pi-rs-p2 \
    --bin p2

[[ -f "$P2_BIN" ]] || die "expected packaged p2 binary not found: $P2_BIN"
printf '%s\n' "$P2_BIN"
