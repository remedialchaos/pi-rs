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

# 0. Syntax check under sh, dash (when present), and bash.
sh -n "$INSTALLER"
command -v dash >/dev/null 2>&1 && dash -n "$INSTALLER"
bash -n "$INSTALLER"

# 1. Solo drop-in install: installs `pi` + `pi-rs` alias under ~/.pi/agent/install-rs
#    without touching ~/.pi/agent/install.
HOME1="$TMP_ROOT/home1"
mkdir -p "$HOME1/.local/bin" "$HOME1/.pi/agent"
printf '{"theme":"dark"}\n' >"$HOME1/.pi/agent/settings.json"

env -i HOME="$HOME1" PATH="$HOME1/.local/bin:/usr/bin:/bin" \
  sh "$INSTALLER" --yes --no-modify-path --no-start --release-base-url "$BASE_URL"

assert_eq "$("$HOME1/.local/bin/pi" --version)" "pi v9.9.9" "fresh install pi --version"
assert_eq "$("$HOME1/.local/bin/pi-rs" --version)" "pi v9.9.9" "fresh install pi-rs alias --version"
assert_eq "$("$HOME1/.local/bin/p2" --version)" "p2 v9.9.9" "fresh install p2 --version"
assert_eq "$(cat "$HOME1/.pi/agent/install-rs/current-version")" "v9.9.9" "current-version file"
[[ ! -e "$HOME1/.pi/agent/install" ]] || fail "solo install touched upstream ~/.pi/agent/install"
grep -q '"kind": "pi-rs-managed-install"' "$HOME1/.pi/agent/install-rs/pi-rs-install.json" \
  || fail "marker missing kind"
grep -q '"installMode": "drop-in"' "$HOME1/.pi/agent/install-rs/pi-rs-install.json" \
  || fail "marker missing drop-in installMode"

# 2. Upgrade/reinstall to v9.9.10 replaces symlinks and prunes v9.9.9 release dir.
env -i HOME="$HOME1" PATH="$HOME1/.local/bin:/usr/bin:/bin" \
  sh "$INSTALLER" --yes --no-modify-path --no-start --version 9.9.10 --release-base-url "$BASE_URL"

assert_eq "$("$HOME1/.local/bin/pi" --version)" "pi v9.9.10" "upgraded pi --version"
assert_eq "$("$HOME1/.local/bin/pi-rs" --version)" "pi v9.9.10" "upgraded pi-rs alias --version"
assert_eq "$(cat "$HOME1/.pi/agent/install-rs/current-version")" "v9.9.10" "upgraded current-version"
[[ ! -d "$HOME1/.pi/agent/install-rs/releases/v9.9.9" ]] || fail "old release v9.9.9 was not pruned"

# 3. --core-only removes previously linked non-core managed binaries and keeps pi + pi-rs.
env -i HOME="$HOME1" PATH="$HOME1/.local/bin:/usr/bin:/bin" \
  sh "$INSTALLER" --yes --no-modify-path --no-start --version v9.9.10 --core-only --release-base-url "$BASE_URL"

[[ -L "$HOME1/.local/bin/pi" ]] || fail "--core-only missing pi symlink"
[[ -L "$HOME1/.local/bin/pi-rs" ]] || fail "--core-only missing pi-rs alias"
[[ ! -e "$HOME1/.local/bin/p2" ]] || fail "--core-only left stale p2 symlink"

# 4. --uninstall removes managed install and symlinks (including pi-rs alias) while preserving user data.
env -i HOME="$HOME1" PATH="$HOME1/.local/bin:/usr/bin:/bin" \
  sh "$INSTALLER" --uninstall

[[ ! -e "$HOME1/.local/bin/pi" ]] || fail "uninstall left pi symlink"
[[ ! -e "$HOME1/.local/bin/pi-rs" ]] || fail "uninstall left pi-rs symlink"
[[ ! -d "$HOME1/.pi/agent/install-rs" ]] || fail "uninstall left install-rs dir"
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

# 6. Side-by-side coexistence with an existing upstream managed pi installation:
#    - Upstream lives in ~/.pi/agent/install + ~/.pi/agent/bin/pi
#    - Non-interactive install without --as-pi-rs or --force fails cleanly
#    - --as-pi-rs installs `pi-rs` (and non-colliding companions) into ~/.local/bin
#      while leaving upstream `pi` and ~/.pi/agent/install 100% untouched
#    - Reinstalling without --as-pi-rs remembers side-by-side mode
#    - --uninstall removes only pi-rs and ~/.pi/agent/install-rs, leaving upstream intact
HOME3="$TMP_ROOT/home3"
mkdir -p "$HOME3/.local/bin" "$HOME3/.pi/agent/bin" "$HOME3/.pi/agent/install/releases/1.6.0"
printf '{"kind":"pi-managed-install","schemaVersion":1,"layout":"releases-v1"}\n' \
  >"$HOME3/.pi/agent/install/managed-install.json"
cat >"$HOME3/.pi/agent/bin/pi" <<'EOF'
#!/bin/sh
echo "upstream-managed-pi"
EOF
chmod 0755 "$HOME3/.pi/agent/bin/pi"

if env -i HOME="$HOME3" PATH="$HOME3/.pi/agent/bin:$HOME3/.local/bin:/usr/bin:/bin" \
  sh "$INSTALLER" --yes --no-modify-path --no-start --version v9.9.9 --release-base-url "$BASE_URL" 2>"$TMP_ROOT/conflict.err"; then
  fail "unmanaged pi conflict should fail without --as-pi-rs or --force"
fi
grep -qi -- "--as-pi-rs" "$TMP_ROOT/conflict.err" || fail "conflict error should suggest --as-pi-rs"

env -i HOME="$HOME3" PATH="$HOME3/.pi/agent/bin:$HOME3/.local/bin:/usr/bin:/bin" \
  sh "$INSTALLER" --yes --as-pi-rs --no-modify-path --no-start --version v9.9.9 --release-base-url "$BASE_URL"

assert_eq "$("$HOME3/.pi/agent/bin/pi")" "upstream-managed-pi" "side-by-side install modified upstream pi"
assert_eq "$("$HOME3/.pi/agent/bin/pi-rs" --version)" "pi v9.9.9" "side-by-side install pi-rs --version"
[[ -f "$HOME3/.pi/agent/install/managed-install.json" ]] || fail "side-by-side touched upstream managed-install.json"
grep -q '"installMode": "side-by-side"' "$HOME3/.pi/agent/install-rs/pi-rs-install.json" \
  || fail "marker did not record side-by-side mode"

# Reinstall/upgrade remembers side-by-side mode without touching upstream pi.
env -i HOME="$HOME3" PATH="$HOME3/.pi/agent/bin:$HOME3/.local/bin:/usr/bin:/bin" \
  sh "$INSTALLER" --yes --no-modify-path --no-start --version v9.9.10 --release-base-url "$BASE_URL"

assert_eq "$("$HOME3/.pi/agent/bin/pi")" "upstream-managed-pi" "side-by-side upgrade modified upstream pi"
assert_eq "$("$HOME3/.pi/agent/bin/pi-rs" --version)" "pi v9.9.10" "side-by-side upgrade pi-rs --version"

# Uninstall cleans only pi-rs + install-rs, leaving upstream pi and install dir intact.
env -i HOME="$HOME3" PATH="$HOME3/.pi/agent/bin:$HOME3/.local/bin:/usr/bin:/bin" \
  sh "$INSTALLER" --uninstall
assert_eq "$("$HOME3/.pi/agent/bin/pi")" "upstream-managed-pi" "uninstall touched upstream pi"
[[ -f "$HOME3/.pi/agent/install/managed-install.json" ]] || fail "uninstall removed upstream managed-install.json"
[[ ! -e "$HOME3/.pi/agent/bin/pi-rs" ]] || fail "uninstall left pi-rs symlink"
[[ ! -d "$HOME3/.pi/agent/install-rs" ]] || fail "uninstall left install-rs dir"

# 7. --force + --bin-dir + --binaries installs drop-in `pi` alongside unmanaged `pi` cleanly.
HOME4="$TMP_ROOT/home4"
mkdir -p "$HOME4/other-bin" "$HOME4/custom-bin"
cat >"$HOME4/other-bin/pi" <<'EOF'
#!/bin/sh
echo "other-pi"
EOF
chmod 0755 "$HOME4/other-bin/pi"

env -i HOME="$HOME4" PATH="$HOME4/other-bin:/usr/bin:/bin" \
  sh "$INSTALLER" --yes --force --no-modify-path --no-start --bin-dir "$HOME4/custom-bin" --binaries pi,p2 --version v9.9.9 --release-base-url "$BASE_URL"

assert_eq "$("$HOME4/custom-bin/pi" --version)" "pi v9.9.9" "forced custom-bin pi"
assert_eq "$("$HOME4/custom-bin/p2" --version)" "p2 v9.9.9" "forced custom-bin p2"
[[ ! -e "$HOME4/custom-bin/pi-env" ]] || fail "--binaries pi,p2 installed unexpected pi-env"
assert_eq "$("$HOME4/other-bin/pi")" "other-pi" "forced install modified unmanaged pi"

# 8. Piped stdin (`curl ... | sh` simulation) works with env vars.
HOME5="$TMP_ROOT/home5"
mkdir -p "$HOME5/.local/bin"
env -i HOME="$HOME5" PATH="$HOME5/.local/bin:/usr/bin:/bin" \
  PI_YES=1 PI_NO_MODIFY_PATH=1 PI_NO_START=1 PI_CORE_ONLY=1 PI_NO_ALIAS=1 PI_RELEASE_BASE_URL="$BASE_URL" \
  sh < "$INSTALLER"
assert_eq "$("$HOME5/.local/bin/pi" --version)" "pi v9.9.9" "piped sh install failed"
[[ ! -e "$HOME5/.local/bin/pi-rs" ]] || fail "PI_NO_ALIAS=1 still created pi-rs alias"

echo "PASS: all 8 install.sh end-to-end checks passed"
