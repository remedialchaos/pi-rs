#!/usr/bin/env bash
set -euo pipefail

# Hermetic test suite for install.sh.
# Runs entirely inside a temporary HOME and local HTTP server; never touches
# the user's real HOME, PATH, or installed binaries.

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALLER="$REPO_ROOT/install.sh"

TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/pi-rs-install-test.XXXXXX")"
SERVER_PID=""
cleanup() {
  if [[ -n "$SERVER_PID" ]]; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  rm -rf "$TMP_ROOT"
}
trap cleanup EXIT

detect_expected_target() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os:$arch" in
    Linux:x86_64|Linux:amd64) echo "x86_64-unknown-linux-gnu" ;;
    Linux:aarch64|Linux:arm64) echo "aarch64-unknown-linux-gnu" ;;
    Darwin:arm64|Darwin:aarch64) echo "aarch64-apple-darwin" ;;
    *) echo "unsupported host for install.sh test: $os:$arch" >&2; exit 1 ;;
  esac
}

TARGET="$(detect_expected_target)"
HTTP_ROOT="$TMP_ROOT/http"
mkdir -p "$HTTP_ROOT"

build_fake_release() {
  local ver="$1"
  local stage_name="pi-rs-${ver}-${TARGET}"
  local rel_dir="$HTTP_ROOT/$ver"
  local work="$TMP_ROOT/build-$ver"
  mkdir -p "$work/$stage_name" "$rel_dir"

  for bin in pi pi-ai pi-env pi-evals p3 p2 pi-voice-server; do
    cat >"$work/$stage_name/$bin" <<EOF
#!/bin/sh
if [ "\${1:-}" = "--version" ]; then
  echo "$bin $ver"
  exit 0
fi
echo "mock $bin $ver"
EOF
    chmod 0755 "$work/$stage_name/$bin"
  done
  printf 'MIT\n' >"$work/$stage_name/LICENSE"
  printf '# pi-rs\n' >"$work/$stage_name/README.md"

  tar -C "$work" -czf "$rel_dir/${stage_name}.tar.gz" "$stage_name"
  (
    cd "$rel_dir"
    if command -v sha256sum >/dev/null 2>&1; then
      sha256sum "${stage_name}.tar.gz" > SHA256SUMS.txt
    else
      shasum -a 256 "${stage_name}.tar.gz" > SHA256SUMS.txt
    fi
  )
}

build_fake_release "v9.9.9"
build_fake_release "v9.9.10"
printf 'v9.9.9\n' >"$HTTP_ROOT/latest-version.txt"

# Start local HTTP server on an ephemeral port.
PORT_FILE="$TMP_ROOT/port"
python3 - "$HTTP_ROOT" "$PORT_FILE" <<'PY' &
import functools, http.server, socketserver, sys
root, port_file = sys.argv[1], sys.argv[2]
class QuietHandler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, format, *args):
        pass
handler = functools.partial(QuietHandler, directory=root)
httpd = socketserver.TCPServer(("127.0.0.1", 0), handler)
port = httpd.server_address[1]
with open(port_file, "w") as f:
    f.write(str(port))
httpd.serve_forever()
PY
SERVER_PID=$!

for _ in $(seq 1 50); do
  [[ -s "$PORT_FILE" ]] && break
  sleep 0.05
done
PORT="$(cat "$PORT_FILE")"
BASE_URL="http://127.0.0.1:${PORT}"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

assert_eq() {
  local got="$1" expected="$2" msg="$3"
  [[ "$got" == "$expected" ]] || fail "$msg (expected '$expected', got '$got')"
}

# 0. Syntax check under both sh and bash.
sh -n "$INSTALLER"
bash -n "$INSTALLER"

# 1. Fresh install via latest-version.txt, SHA-256 verification, marker, and smoke test.
HOME1="$TMP_ROOT/home1"
mkdir -p "$HOME1/.local/bin" "$HOME1/.pi/agent"
printf '{"theme":"dark"}\n' >"$HOME1/.pi/agent/settings.json"

env -i HOME="$HOME1" PATH="$HOME1/.local/bin:/usr/bin:/bin" \
  sh "$INSTALLER" --yes --no-modify-path --no-start --release-base-url "$BASE_URL"

assert_eq "$("$HOME1/.local/bin/pi" --version)" "pi v9.9.9" "fresh install pi --version"
assert_eq "$("$HOME1/.local/bin/p2" --version)" "p2 v9.9.9" "fresh install p2 --version"
assert_eq "$(cat "$HOME1/.pi/agent/install/current-version")" "v9.9.9" "current-version file"
grep -q '"kind": "pi-rs-managed-install"' "$HOME1/.pi/agent/install/pi-rs-install.json" \
  || fail "marker missing kind"

# 2. Upgrade/reinstall to v9.9.10 replaces symlinks and prunes v9.9.9 release dir.
env -i HOME="$HOME1" PATH="$HOME1/.local/bin:/usr/bin:/bin" \
  sh "$INSTALLER" --yes --no-modify-path --no-start --version 9.9.10 --release-base-url "$BASE_URL"

assert_eq "$("$HOME1/.local/bin/pi" --version)" "pi v9.9.10" "upgraded pi --version"
assert_eq "$(cat "$HOME1/.pi/agent/install/current-version")" "v9.9.10" "upgraded current-version"
[[ ! -d "$HOME1/.pi/agent/install/releases/v9.9.9" ]] || fail "old release v9.9.9 was not pruned"

# 3. --core-only removes previously linked non-core managed binaries and keeps pi.
env -i HOME="$HOME1" PATH="$HOME1/.local/bin:/usr/bin:/bin" \
  sh "$INSTALLER" --yes --no-modify-path --no-start --version v9.9.10 --core-only --release-base-url "$BASE_URL"

[[ -L "$HOME1/.local/bin/pi" ]] || fail "--core-only missing pi symlink"
[[ ! -e "$HOME1/.local/bin/p2" ]] || fail "--core-only left stale p2 symlink"

# 4. --uninstall removes managed install and symlinks while preserving ~/.pi/agent/ user data.
env -i HOME="$HOME1" PATH="$HOME1/.local/bin:/usr/bin:/bin" \
  sh "$INSTALLER" --uninstall

[[ ! -e "$HOME1/.local/bin/pi" ]] || fail "uninstall left pi symlink"
[[ ! -d "$HOME1/.pi/agent/install" ]] || fail "uninstall left install dir"
assert_eq "$(cat "$HOME1/.pi/agent/settings.json")" '{"theme":"dark"}' "uninstall modified user settings.json"

# 5. Tampered SHA256SUMS.txt MUST fail loudly and leave no release installed.
BAD_DIR="$HTTP_ROOT/v9.9.9-bad"
mkdir -p "$BAD_DIR"
cp "$HTTP_ROOT/v9.9.9/pi-rs-v9.9.9-${TARGET}.tar.gz" "$BAD_DIR/pi-rs-v9.9.9-bad-${TARGET}.tar.gz"
printf '0000000000000000000000000000000000000000000000000000000000000000  pi-rs-v9.9.9-bad-%s.tar.gz\n' "$TARGET" >"$BAD_DIR/SHA256SUMS.txt"

HOME2="$TMP_ROOT/home2"
mkdir -p "$HOME2/.local/bin"
if env -i HOME="$HOME2" PATH="$HOME2/.local/bin:/usr/bin:/bin" \
  sh "$INSTALLER" --yes --no-modify-path --no-start --version v9.9.9-bad --release-base-url "$BASE_URL" 2>"$TMP_ROOT/bad.err"; then
  fail "tampered checksum unexpectedly succeeded"
fi
grep -qi "checksum mismatch" "$TMP_ROOT/bad.err" || fail "missing checksum mismatch error: $(cat "$TMP_ROOT/bad.err")"
[[ ! -e "$HOME2/.local/bin/pi" ]] || fail "failed checksum install still linked pi"

# 6. Unmanaged `pi` on PATH blocks non-interactive install unless --force is passed,
#    and uninstall never touches the unmanaged binary.
HOME3="$TMP_ROOT/home3"
mkdir -p "$HOME3/other-bin" "$HOME3/custom-bin"
cat >"$HOME3/other-bin/pi" <<'EOF'
#!/bin/sh
echo "upstream-node-pi"
EOF
chmod 0755 "$HOME3/other-bin/pi"

if env -i HOME="$HOME3" PATH="$HOME3/other-bin:/usr/bin:/bin" \
  sh "$INSTALLER" --yes --no-modify-path --no-start --version v9.9.9 --release-base-url "$BASE_URL" 2>"$TMP_ROOT/conflict.err"; then
  fail "unmanaged pi conflict should fail without --force"
fi
grep -qi "already on PATH" "$TMP_ROOT/conflict.err" || fail "missing conflict error"

env -i HOME="$HOME3" PATH="$HOME3/other-bin:/usr/bin:/bin" \
  sh "$INSTALLER" --yes --force --no-modify-path --no-start --bin-dir "$HOME3/custom-bin" --binaries pi,p2 --version v9.9.9 --release-base-url "$BASE_URL"

assert_eq "$("$HOME3/custom-bin/pi" --version)" "pi v9.9.9" "forced custom-bin pi"
assert_eq "$("$HOME3/custom-bin/p2" --version)" "p2 v9.9.9" "forced custom-bin p2"
[[ ! -e "$HOME3/custom-bin/pi-env" ]] || fail "--binaries pi,p2 installed unexpected pi-env"
assert_eq "$("$HOME3/other-bin/pi")" "upstream-node-pi" "unmanaged pi was modified"

env -i HOME="$HOME3" PATH="$HOME3/other-bin:/usr/bin:/bin" \
  sh "$INSTALLER" --uninstall
assert_eq "$("$HOME3/other-bin/pi")" "upstream-node-pi" "uninstall touched unmanaged pi"

# 7. Piped stdin (`curl ... | sh` simulation) works with env vars.
HOME4="$TMP_ROOT/home4"
mkdir -p "$HOME4/.local/bin"
env -i HOME="$HOME4" PATH="$HOME4/.local/bin:/usr/bin:/bin" \
  PI_YES=1 PI_NO_MODIFY_PATH=1 PI_NO_START=1 PI_CORE_ONLY=1 PI_RELEASE_BASE_URL="$BASE_URL" \
  sh < "$INSTALLER"
assert_eq "$("$HOME4/.local/bin/pi" --version)" "pi v9.9.9" "piped sh install failed"

echo "PASS: all 7 install.sh end-to-end checks passed"
