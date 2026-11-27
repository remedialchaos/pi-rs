#!/usr/bin/env bash
# Packages Pi.app: the macOS app with the native p2 server it runs.
#
# The bundle holds the app in Contents/MacOS/pi-mac and the p2 server in Contents/Resources/bin/p2,
# so the app finds its own server (ServerConfig::bundled looks for Resources/bin/p2) and never needs
# Node or a checkout. Output: build/Pi.app and, with --archive, a tar of the bundle for the updater.
#
#   ./scripts/package-p2-mac.sh                 a local bundle, ad-hoc signed
#   ./scripts/package-p2-mac.sh --release       an optimized build
#   ./scripts/package-p2-mac.sh --archive       also write build/dist/Pi.tar for the updater
#
# Environment (all optional):
#   SIGN_IDENTITY   codesign identity, e.g. "Developer ID Application: … (TEAMID)"; default: ad-hoc
#   FEED_URL        the appcast the app checks; default: the nightly channel
#   SPARKLE_PUBLIC_KEY  its EdDSA public key: the app gets the feed and updates itself
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: package-p2-mac.sh [--release] [--archive] [--locked] [--offline]

  --release   build optimized binaries
  --archive   also write build/dist/Pi.tar, the archive the updater unpacks
  --locked    pass --locked to cargo
  --offline   pass --offline to cargo
EOF
}

die() {
    printf 'package-p2-mac: %s\n' "$1" >&2
    exit 1
}

RELEASE=0
ARCHIVE=0
LOCKED=0
OFFLINE=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --release) RELEASE=1 ;;
        --archive) ARCHIVE=1 ;;
        --locked) LOCKED=1 ;;
        --offline) OFFLINE=1 ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; die "unknown argument: $1" ;;
    esac
    shift
done

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

[[ "$(uname -s)" == "Darwin" ]] || die "the app bundle can only be built on macOS"

SIGN_IDENTITY="${SIGN_IDENTITY:--}"
FEED_URL="${FEED_URL:-https://pkg.earendil.com/-/apps/pi-mac/nightly/appcast.xml}"

BUILD_DIR="$REPO_ROOT/build"
APP="$BUILD_DIR/Pi.app"
DIST_DIR="$BUILD_DIR/dist"
PROFILE_DIR="debug"
CARGO_FLAGS=()
[[ "$LOCKED" -eq 1 ]] && CARGO_FLAGS+=(--locked)
[[ "$OFFLINE" -eq 1 ]] && CARGO_FLAGS+=(--offline)
PROFILE_FLAGS=()
[[ "$RELEASE" -eq 1 ]] && PROFILE_FLAGS+=(--release) && PROFILE_DIR="release"

TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
TARGET_DIR="${TARGET_DIR%/}"
PROFILE_TARGET_DIR="$TARGET_DIR/$PROFILE_DIR"

echo "Building the p2 server and the app ($PROFILE_DIR)..." >&2
cargo build \
    "${CARGO_FLAGS[@]}" \
    "${PROFILE_FLAGS[@]}" \
    -p pi-rs-p2 \
    --bin p2
cargo build \
    "${CARGO_FLAGS[@]}" \
    "${PROFILE_FLAGS[@]}" \
    -p pi-rs-p2-mac \
    --bin pi-mac

P2_BIN="$PROFILE_TARGET_DIR/p2"
APP_BIN="$PROFILE_TARGET_DIR/pi-mac"
[[ -x "$P2_BIN" ]] || die "expected the p2 binary at $P2_BIN"
[[ -x "$APP_BIN" ]] || die "expected the app binary at $APP_BIN"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources/bin"
cp "$APP_BIN" "$APP/Contents/MacOS/pi-mac"
cp "$P2_BIN" "$APP/Contents/Resources/bin/p2"
cp "$REPO_ROOT/crates/pi-rs-p2-mac/resources/Info.plist" "$APP/Contents/Info.plist"

plist="$APP/Contents/Info.plist"
set_plist() {
    /usr/libexec/PlistBuddy -c "Delete :$1" "$plist" >/dev/null 2>&1 || true
    /usr/libexec/PlistBuddy -c "Add :$1 $2 $3" "$plist"
}
sha="$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || echo unknown)"
version="$(TZ=UTC git -C "$REPO_ROOT" log -1 --format=%cd --date=format-local:%Y%m%d%H%M 2>/dev/null || echo 0)"
short_version="$(TZ=UTC git -C "$REPO_ROOT" log -1 --format=%cd --date=format-local:%Y.%m.%d 2>/dev/null || echo 0.0.0)"
set_plist CFBundleVersion string "$version"
set_plist CFBundleShortVersionString string "$short_version"
set_plist PiCommit string "$sha"
if [[ -n "${SPARKLE_PUBLIC_KEY:-}" ]]; then
    set_plist SUFeedURL string "$FEED_URL"
    set_plist SUPublicEDKey string "$SPARKLE_PUBLIC_KEY"
    set_plist SUEnableAutomaticChecks bool true
fi

# Signing, inside out: the p2 server first, then the app.
sign=(codesign --force --sign "$SIGN_IDENTITY")
if [[ "$SIGN_IDENTITY" != "-" ]]; then
    sign+=(--options runtime --timestamp)
    codesign "${sign[@]}" "$APP/Contents/Resources/bin/p2"
fi
codesign "${sign[@]}" "$APP"
codesign --verify --deep --strict "$APP"

echo "Packaged $APP" >&2

if [[ "$ARCHIVE" -eq 1 ]]; then
    mkdir -p "$DIST_DIR"
    archive="$DIST_DIR/Pi.tar"
    rm -f "$archive"
    # The archive holds the bundle, so the updater unpacks it in place.
    tar --format ustar -cf "$archive" -C "$BUILD_DIR" "Pi.app"
    echo "Wrote $archive" >&2
fi
