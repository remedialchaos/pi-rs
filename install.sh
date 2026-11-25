#!/bin/sh
# pi-rs installer (Linux / macOS)
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/remedialchaos/pi-rs/main/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/remedialchaos/pi-rs/main/install.sh | sh -s -- [OPTIONS]
#
# Mirrors upstream pi's managed installer experience (interactive install,
# reinstall/update, uninstall, PATH detection, and profile setup) using native
# Rust release archives or --from-source builds with zero Node.js dependency.

set -eu

PI_CMD="pi"
PI_DEFAULT_REPO="remedialchaos/pi-rs"
PI_MARKER_NAME="pi-rs-install.json"
PI_ALL_BINARIES="pi pi-ai pi-env pi-evals p3 p2 pi-voice-server pi-mac"
PI_ESC=$(printf '\033')
PI_CR=$(printf '\r')
PI_ETX=$(printf '\003')

usage() {
  cat <<'EOF'
pi-rs installer

Usage:
  curl -fsSL https://raw.githubusercontent.com/remedialchaos/pi-rs/main/install.sh | sh
  curl -fsSL https://raw.githubusercontent.com/remedialchaos/pi-rs/main/install.sh | sh -s -- [OPTIONS]
  ./install.sh [OPTIONS]

Options:
  -v, --version <TAG>        Release tag to install (e.g. v1.6.0) or "latest" (default: latest)
      --bin-dir <DIR>        Directory for installed binary symlinks (default: auto-detected on PATH)
      --install-dir <DIR>    Managed install root (default: ~/.pi/agent/install)
      --all                  Install all binaries from the release (default)
      --core-only            Install only the `pi` coding agent binary
      --binaries <LIST>      Comma-separated list of binaries to install (e.g. pi,p2,pi-env)
      --from-source          Build from source using Cargo instead of downloading a release archive
      --git-ref <REF>        Git branch, tag, or commit to build when using --from-source
      --uninstall            Uninstall managed pi-rs binaries (preserves ~/.pi/agent/ user data)
      --reinstall            Reinstall or update without prompting for action
  -y, --yes                  Non-interactive mode; accept default actions without prompts
      --force                Allow installing alongside an unmanaged `pi` in non-interactive mode
      --no-modify-path       Do not prompt to edit shell profile files
      --no-start             Do not prompt to launch `pi` after installation
      --repo <OWNER/REPO>    GitHub repository (default: remedialchaos/pi-rs)
      --release-base-url <U> Base URL for release downloads (default: GitHub Releases)
      --archive <PATH>       Install from a local .tar.gz release archive
      --checksums <PATH>     Local SHA256SUMS.txt used to verify --archive
  -h, --help                 Show this help message and exit

Environment variable equivalents (useful when piping to `sh` without `-s --`):
  PI_VERSION, PI_BIN_DIR, PI_INSTALL_DIR, PI_CORE_ONLY, PI_BINARIES,
  PI_FROM_SOURCE, PI_GIT_REF, PI_UNINSTALL, PI_YES, PI_FORCE,
  PI_NO_MODIFY_PATH, PI_NO_START, PI_REPO, PI_RELEASE_BASE_URL,
  PI_ARCHIVE_PATH, PI_CHECKSUMS_PATH, PI_CODING_AGENT_DIR
EOF
}

die() {
  printf 'pi-rs installer: error: %s\n' "$*" >&2
  exit 1
}

warn() {
  printf 'pi-rs installer: warning: %s\n' "$*" >&2
}

info() {
  printf '%s\n' "$*"
}

is_truthy() {
  case "${1:-}" in
    1|true|TRUE|yes|YES|on|ON) return 0 ;;
    *) return 1 ;;
  esac
}

parse_args() {
  PI_VERSION="${PI_VERSION:-latest}"
  PI_BIN_DIR="${PI_BIN_DIR:-}"
  PI_INSTALL_DIR="${PI_INSTALL_DIR:-}"
  PI_BINARIES="${PI_BINARIES:-}"
  PI_GIT_REF="${PI_GIT_REF:-}"
  PI_REPO="${PI_REPO:-$PI_DEFAULT_REPO}"
  PI_RELEASE_BASE_URL="${PI_RELEASE_BASE_URL:-}"
  PI_ARCHIVE_PATH="${PI_ARCHIVE_PATH:-}"
  PI_CHECKSUMS_PATH="${PI_CHECKSUMS_PATH:-}"

  PI_OPT_CORE_ONLY=0
  PI_OPT_FROM_SOURCE=0
  PI_OPT_UNINSTALL=0
  PI_OPT_REINSTALL=0
  PI_OPT_YES=0
  PI_OPT_FORCE=0
  PI_OPT_NO_MODIFY_PATH=0
  PI_OPT_NO_START=0

  is_truthy "${PI_CORE_ONLY:-0}" && PI_OPT_CORE_ONLY=1
  is_truthy "${PI_FROM_SOURCE:-0}" && PI_OPT_FROM_SOURCE=1
  is_truthy "${PI_UNINSTALL:-0}" && PI_OPT_UNINSTALL=1
  is_truthy "${PI_YES:-0}" && PI_OPT_YES=1
  is_truthy "${PI_FORCE:-0}" && PI_OPT_FORCE=1
  is_truthy "${PI_NO_MODIFY_PATH:-0}" && PI_OPT_NO_MODIFY_PATH=1
  is_truthy "${PI_NO_START:-0}" && PI_OPT_NO_START=1

  while [ "$#" -gt 0 ]; do
    case "$1" in
      -v|--version)
        [ "$#" -ge 2 ] && [ -n "${2:-}" ] || die "$1 requires a version argument"
        PI_VERSION="$2"
        shift 2
        ;;
      --version=*)
        PI_VERSION="${1#*=}"
        [ -n "$PI_VERSION" ] || die "--version requires a non-empty value"
        shift
        ;;
      --bin-dir)
        [ "$#" -ge 2 ] && [ -n "${2:-}" ] || die "--bin-dir requires a directory argument"
        PI_BIN_DIR="$2"
        shift 2
        ;;
      --bin-dir=*)
        PI_BIN_DIR="${1#*=}"
        [ -n "$PI_BIN_DIR" ] || die "--bin-dir requires a non-empty value"
        shift
        ;;
      --install-dir)
        [ "$#" -ge 2 ] && [ -n "${2:-}" ] || die "--install-dir requires a directory argument"
        PI_INSTALL_DIR="$2"
        shift 2
        ;;
      --install-dir=*)
        PI_INSTALL_DIR="${1#*=}"
        [ -n "$PI_INSTALL_DIR" ] || die "--install-dir requires a non-empty value"
        shift
        ;;
      --all)
        PI_OPT_CORE_ONLY=0
        PI_BINARIES=""
        shift
        ;;
      --core-only)
        PI_OPT_CORE_ONLY=1
        PI_BINARIES="pi"
        shift
        ;;
      --binaries)
        [ "$#" -ge 2 ] && [ -n "${2:-}" ] || die "--binaries requires a comma-separated list"
        PI_OPT_CORE_ONLY=0
        PI_BINARIES="$2"
        shift 2
        ;;
      --binaries=*)
        PI_OPT_CORE_ONLY=0
        PI_BINARIES="${1#*=}"
        [ -n "$PI_BINARIES" ] || die "--binaries requires a non-empty value"
        shift
        ;;
      --from-source)
        PI_OPT_FROM_SOURCE=1
        shift
        ;;
      --git-ref)
        [ "$#" -ge 2 ] && [ -n "${2:-}" ] || die "--git-ref requires a git ref argument"
        PI_GIT_REF="$2"
        shift 2
        ;;
      --git-ref=*)
        PI_GIT_REF="${1#*=}"
        [ -n "$PI_GIT_REF" ] || die "--git-ref requires a non-empty value"
        shift
        ;;
      --uninstall)
        PI_OPT_UNINSTALL=1
        shift
        ;;
      --reinstall)
        PI_OPT_REINSTALL=1
        shift
        ;;
      -y|--yes)
        PI_OPT_YES=1
        shift
        ;;
      --force)
        PI_OPT_FORCE=1
        shift
        ;;
      --no-modify-path)
        PI_OPT_NO_MODIFY_PATH=1
        shift
        ;;
      --no-start)
        PI_OPT_NO_START=1
        shift
        ;;
      --repo)
        [ "$#" -ge 2 ] && [ -n "${2:-}" ] || die "--repo requires OWNER/REPO"
        PI_REPO="$2"
        shift 2
        ;;
      --repo=*)
        PI_REPO="${1#*=}"
        [ -n "$PI_REPO" ] || die "--repo requires a non-empty value"
        shift
        ;;
      --release-base-url)
        [ "$#" -ge 2 ] && [ -n "${2:-}" ] || die "--release-base-url requires a URL"
        PI_RELEASE_BASE_URL="$2"
        shift 2
        ;;
      --release-base-url=*)
        PI_RELEASE_BASE_URL="${1#*=}"
        [ -n "$PI_RELEASE_BASE_URL" ] || die "--release-base-url requires a non-empty value"
        shift
        ;;
      --archive)
        [ "$#" -ge 2 ] && [ -n "${2:-}" ] || die "--archive requires a file path"
        PI_ARCHIVE_PATH="$2"
        shift 2
        ;;
      --archive=*)
        PI_ARCHIVE_PATH="${1#*=}"
        [ -n "$PI_ARCHIVE_PATH" ] || die "--archive requires a non-empty value"
        shift
        ;;
      --checksums)
        [ "$#" -ge 2 ] && [ -n "${2:-}" ] || die "--checksums requires a file path"
        PI_CHECKSUMS_PATH="$2"
        shift 2
        ;;
      --checksums=*)
        PI_CHECKSUMS_PATH="${1#*=}"
        [ -n "$PI_CHECKSUMS_PATH" ] || die "--checksums requires a non-empty value"
        shift
        ;;
      -h|--help)
        usage
        exit 0
        ;;
      *)
        printf 'pi-rs installer: error: unknown option: %s\n\n' "$1" >&2
        usage >&2
        exit 2
        ;;
    esac
  done

  if [ "$PI_OPT_CORE_ONLY" -eq 1 ] && [ -z "$PI_BINARIES" ]; then
    PI_BINARIES="pi"
  fi
}

# Normalize requested binaries into a validated space-separated list (or empty for "all").
resolve_requested_binaries() {
  if [ -z "${PI_BINARIES:-}" ]; then
    printf ''
    return 0
  fi

  req_out=""
  old_ifs=${IFS- }
  IFS=,
  for raw_bin in $PI_BINARIES; do
    IFS=$old_ifs
    bin_name=$(printf '%s' "$raw_bin" | tr -d '[:space:]')
    [ -n "$bin_name" ] || continue
    case " $PI_ALL_BINARIES " in
      *" $bin_name "*) ;;
      *) die "unknown binary '$bin_name' (supported: $PI_ALL_BINARIES)" ;;
    esac
    case " $req_out " in
      *" $bin_name "*) ;;
      *)
        if [ -z "$req_out" ]; then
          req_out="$bin_name"
        else
          req_out="$req_out $bin_name"
        fi
        ;;
    esac
    IFS=,
  done
  IFS=$old_ifs

  [ -n "$req_out" ] || die "--binaries produced an empty binary list"
  printf '%s' "$req_out"
}

detect_target() {
  os_name=$(uname -s 2>/dev/null || echo unknown)
  arch_name=$(uname -m 2>/dev/null || echo unknown)

  case "$os_name" in
    Linux)
      case "$arch_name" in
        x86_64|amd64) printf 'x86_64-unknown-linux-gnu' ;;
        aarch64|arm64) printf 'aarch64-unknown-linux-gnu' ;;
        *)
          die "no prebuilt Linux release for architecture '$arch_name'; re-run with --from-source"
          ;;
      esac
      ;;
    Darwin)
      case "$arch_name" in
        arm64|aarch64) printf 'aarch64-apple-darwin' ;;
        x86_64)
          die "no prebuilt macOS x86_64 release is published; re-run with --from-source"
          ;;
        *)
          die "no prebuilt macOS release for architecture '$arch_name'; re-run with --from-source"
          ;;
      esac
      ;;
    MINGW*|MSYS*|CYGWIN*)
      die "on Windows, use PowerShell: powershell -c \"irm https://raw.githubusercontent.com/$PI_REPO/main/install.ps1 | iex\""
      ;;
    *)
      die "unsupported operating system '$os_name'; re-run with --from-source"
      ;;
  esac
}

# WSL appends the Windows PATH, so command -v can resolve a Windows pi.exe.
# Leave Windows installs alone when installing pi-rs for Linux.
path_is_on_windows_drive() {
  command -v wslpath >/dev/null 2>&1 || return 1
  case "$(wslpath -w "$1" 2>/dev/null || true)" in
    [A-Za-z]:\\*) return 0 ;;
  esac
  return 1
}

resolve_path() {
  rp_target="$1"
  if command -v readlink >/dev/null 2>&1; then
    rp_res=$(readlink -f "$rp_target" 2>/dev/null || true)
    if [ -n "$rp_res" ]; then
      printf '%s' "$rp_res"
      return 0
    fi
  fi
  if [ -d "$rp_target" ]; then
    (cd "$rp_target" 2>/dev/null && pwd -P) || printf '%s' "$rp_target"
  else
    rp_dir=$(dirname "$rp_target")
    rp_base=$(basename "$rp_target")
    if [ -d "$rp_dir" ]; then
      rp_abs_dir=$( (cd "$rp_dir" 2>/dev/null && pwd -P) || printf '%s' "$rp_dir" )
      printf '%s/%s' "${rp_abs_dir%/}" "$rp_base"
    else
      printf '%s' "$rp_target"
    fi
  fi
}

path_is_writable_or_creatable() {
  pw_dir="$1"
  while [ -n "$pw_dir" ] && [ ! -e "$pw_dir" ]; do
    pw_parent=${pw_dir%/*}
    [ "$pw_parent" != "$pw_dir" ] || break
    pw_dir=${pw_parent:-/}
  done
  [ -d "$pw_dir" ] && [ -w "$pw_dir" ]
}

marker_read_field() {
  mrf_file="$1"
  mrf_key="$2"
  [ -f "$mrf_file" ] || return 1
  sed -n "s/^[[:space:]]*\"$mrf_key\"[[:space:]]*:[[:space:]]*\"\([^\"]*\)\".*/\1/p" "$mrf_file" | head -n 1
}

command_belongs_to_managed_root() {
  cb_cmd="$1"
  cb_root="$2"
  [ -n "$cb_cmd" ] && [ -n "$cb_root" ] || return 1
  [ -e "$cb_cmd" ] || [ -L "$cb_cmd" ] || return 1

  cb_real_cmd=$(resolve_path "$cb_cmd")
  cb_real_root=$(resolve_path "$cb_root")
  case "$cb_real_cmd" in
    "$cb_real_root"/*) return 0 ;;
  esac
  return 1
}

managed_bin_dir_is_free() {
  mb_dir="$1"
  mb_root="$2"
  if [ ! -e "$mb_dir/$PI_CMD" ] && [ ! -L "$mb_dir/$PI_CMD" ]; then
    return 0
  fi
  command_belongs_to_managed_root "$mb_dir/$PI_CMD" "$mb_root"
}

select_install_dir() {
  if [ -n "$PI_INSTALL_DIR" ]; then
    printf '%s' "${PI_INSTALL_DIR%/}"
    return 0
  fi
  sid_agent_dir="${PI_CODING_AGENT_DIR:-$HOME/.pi/agent}"
  printf '%s/install' "${sid_agent_dir%/}"
}

select_bin_dir() {
  sbd_agent_dir="$1"
  sbd_install_dir="$2"
  sbd_existing_path="$3"

  if [ -n "$PI_BIN_DIR" ]; then
    printf '%s' "${PI_BIN_DIR%/}"
    return 0
  fi

  # Reuse existing managed install's binDir if recorded.
  if [ -f "$sbd_install_dir/$PI_MARKER_NAME" ]; then
    sbd_recorded=$(marker_read_field "$sbd_install_dir/$PI_MARKER_NAME" "binDir" || true)
    if [ -n "$sbd_recorded" ]; then
      printf '%s' "${sbd_recorded%/}"
      return 0
    fi
  fi

  if [ -n "$sbd_existing_path" ] && command_belongs_to_managed_root "$sbd_existing_path" "$sbd_install_dir"; then
    sbd_existing_bin=$(dirname "$sbd_existing_path")
    printf '%s' "${sbd_existing_bin%/}"
    return 0
  fi

  sbd_old_ifs=${IFS- }
  IFS=:
  for sbd_dir in ${PATH:-}; do
    IFS=$sbd_old_ifs
    sbd_dir=${sbd_dir%/}
    case "$sbd_dir" in
      "$sbd_agent_dir/bin"|"$HOME/.local/bin"|"$HOME/bin"|"$HOME/.bin"|"$HOME/local/bin")
        if path_is_writable_or_creatable "$sbd_dir" && managed_bin_dir_is_free "$sbd_dir" "$sbd_install_dir"; then
          printf '%s' "$sbd_dir"
          return 0
        fi
        ;;
    esac
    IFS=:
  done
  IFS=$sbd_old_ifs

  # Homebrew bin directory on macOS / Linuxbrew when writable and free.
  IFS=:
  for sbd_dir in ${PATH:-}; do
    IFS=$sbd_old_ifs
    sbd_dir=${sbd_dir%/}
    if [ -n "$sbd_dir" ] && [ -x "$sbd_dir/brew" ] && [ -w "$sbd_dir" ] && managed_bin_dir_is_free "$sbd_dir" "$sbd_install_dir"; then
      printf '%s' "$sbd_dir"
      return 0
    fi
    IFS=:
  done
  IFS=$sbd_old_ifs

  printf '%s/bin' "${sbd_agent_dir%/}"
}

has_tty() {
  [ "$PI_OPT_YES" -eq 0 ] || return 1
  ( : <>/dev/tty ) 2>/dev/null
}

read_tty_key() {
  rtk_saved=$(stty -g <&3 2>/dev/null || true)
  if [ -n "$rtk_saved" ]; then
    stty -icanon -echo min 1 time 0 <&3 2>/dev/null || true
    rtk_key=$(dd bs=1 count=1 <&3 2>/dev/null || true)
    stty "$rtk_saved" <&3 2>/dev/null || true
    printf '%s' "$rtk_key"
  else
    IFS= read -r rtk_line <&3 || rtk_line=""
    printf '%s' "$(printf '%s' "$rtk_line" | cut -c1)"
  fi
}

default_action() {
  if [ "$PI_OPT_UNINSTALL" -eq 1 ]; then
    printf 'uninstall'
  elif [ "$PI_OPT_REINSTALL" -eq 1 ]; then
    printf 'reinstall'
  elif [ "$PI_HAS_MANAGED" -eq 1 ]; then
    printf 'reinstall'
  elif [ -n "$PI_OTHER_PI_PATH" ]; then
    if [ "$PI_OPT_FORCE" -eq 1 ]; then
      printf 'install'
    else
      printf 'none'
    fi
  else
    printf 'install'
  fi
}

print_action_menu() {
  pam_bold=""
  pam_dim=""
  pam_reset=""
  if [ -t 1 ] && [ "${TERM:-}" != "dumb" ]; then
    pam_bold="${PI_ESC}[1m"
    pam_dim="${PI_ESC}[2m"
    pam_reset="${PI_ESC}[0m"
  fi

  printf '%spi-rs Installer%s\n' "$pam_bold" "$pam_reset"
  printf '%s100%% Rust port of pi%s\n\n' "$pam_dim" "$pam_reset"

  if [ "$PI_HAS_MANAGED" -eq 1 ]; then
    pam_prev_ver=$(marker_read_field "$PI_RESOLVED_INSTALL_DIR/$PI_MARKER_NAME" "version" || true)
    printf 'Existing managed pi-rs installation found in %s' "$PI_RESOLVED_INSTALL_DIR"
    if [ -n "$pam_prev_ver" ]; then
      printf ' (%s)' "$pam_prev_ver"
    fi
    printf '.\n'
    printf 'Target bin directory: %s\n\n' "$PI_RESOLVED_BIN_DIR"
    printf '  [Enter/y] Reinstall or update pi-rs\n'
    printf '  [u]       Uninstall pi-rs (keeps ~/.pi/agent/ config and sessions)\n'
    printf '  [n/Esc]   Cancel\n\n'
  elif [ -n "$PI_OTHER_PI_PATH" ]; then
    printf 'Another `pi` binary is already on PATH: %s\n' "$PI_OTHER_PI_PATH"
    printf 'This installer will not overwrite or remove it.\n'
    printf 'Target bin directory for pi-rs: %s\n\n' "$PI_RESOLVED_BIN_DIR"
    printf '  [y]       Install pi-rs alongside it\n'
    printf '  [Enter/n] Cancel\n\n'
  else
    printf 'Install directory: %s\n' "$PI_RESOLVED_INSTALL_DIR"
    printf 'Bin directory:     %s\n\n' "$PI_RESOLVED_BIN_DIR"
    printf '  [Enter/y] Install pi-rs\n'
    printf '  [n/Esc]   Cancel\n\n'
  fi
}

choose_action() {
  if [ "$PI_OPT_UNINSTALL" -eq 1 ]; then
    PI_ACTION="uninstall"
    return 0
  fi
  if [ "$PI_OPT_REINSTALL" -eq 1 ]; then
    PI_ACTION="reinstall"
    return 0
  fi

  if ! has_tty; then
    if [ -n "$PI_OTHER_PI_PATH" ] && [ "$PI_OPT_FORCE" -eq 0 ]; then
      printf 'Another `pi` binary is already on PATH: %s\n' "$PI_OTHER_PI_PATH" >&2
      printf 'Refusing to install alongside it in non-interactive mode. Pass --force or --bin-dir to install anyway.\n' >&2
      exit 1
    fi
    PI_ACTION=$(default_action)
    return 0
  fi

  exec 3<>/dev/tty
  trap 'exec 3>&-; trap - INT TERM; exit 130' INT TERM
  print_action_menu >&3

  while :; do
    ca_key=$(read_tty_key)
    case "$ca_key" in
      ""|" "|"$PI_CR")
        PI_ACTION=$(default_action)
        break
        ;;
      y|Y)
        if [ -n "$PI_OTHER_PI_PATH" ]; then
          PI_ACTION="install"
        else
          PI_ACTION=$(default_action)
        fi
        break
        ;;
      u|U)
        if [ "$PI_HAS_MANAGED" -eq 1 ]; then
          PI_ACTION="uninstall"
          break
        fi
        ;;
      "$PI_ETX")
        exec 3>&-
        exit 130
        ;;
      n|N|"$PI_ESC")
        PI_ACTION="none"
        break
        ;;
    esac
    printf 'Please choose one of the listed keys.\n' >&3
  done

  exec 3>&-
  trap - INT TERM
}

download_file() {
  df_url="$1"
  df_dest="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$df_url" -o "$df_dest"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$df_dest" "$df_url"
  else
    die "either curl or wget is required to download release artifacts"
  fi
}

resolve_latest_version() {
  if [ -n "$PI_RELEASE_BASE_URL" ]; then
    # Custom release mirror: try <base>/latest-version.txt first if present.
    rlv_tmp="${PI_WORK_DIR}/latest-version.txt"
    if download_file "${PI_RELEASE_BASE_URL%/}/latest-version.txt" "$rlv_tmp" 2>/dev/null; then
      rlv_ver=$(tr -d '\r\n[:space:]' < "$rlv_tmp")
      if [ -n "$rlv_ver" ]; then
        printf '%s' "$rlv_ver"
        return 0
      fi
    fi
  fi

  rlv_latest_url="https://github.com/${PI_REPO}/releases/latest"
  rlv_effective=""
  if command -v curl >/dev/null 2>&1; then
    rlv_effective=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$rlv_latest_url" 2>/dev/null || true)
  fi

  case "$rlv_effective" in
    *"/releases/tag/"*)
      rlv_tag="${rlv_effective##*/releases/tag/}"
      rlv_tag="${rlv_tag%%\?*}"
      rlv_tag="${rlv_tag%%#*}"
      if [ -n "$rlv_tag" ]; then
        printf '%s' "$rlv_tag"
        return 0
      fi
      ;;
  esac

  # Fallback to GitHub Releases API.
  rlv_api_tmp="${PI_WORK_DIR}/release-api.json"
  if download_file "https://api.github.com/repos/${PI_REPO}/releases/latest" "$rlv_api_tmp" 2>/dev/null; then
    rlv_tag=$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$rlv_api_tmp" | head -n 1)
    if [ -n "$rlv_tag" ]; then
      printf '%s' "$rlv_tag"
      return 0
    fi
  fi

  die "no published GitHub release found for ${PI_REPO}. Pass --version <TAG> or build locally with --from-source."
}

normalize_version_tag() {
  nvt_ver="$1"
  case "$nvt_ver" in
    v*|dev-*) printf '%s' "$nvt_ver" ;;
    *) printf 'v%s' "$nvt_ver" ;;
  esac
}

compute_sha256() {
  cs_file="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$cs_file" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$cs_file" | awk '{print $1}'
  elif command -v openssl >/dev/null 2>&1; then
    openssl dgst -sha256 "$cs_file" | awk '{print $NF}'
  else
    die "no SHA-256 verification tool found (need sha256sum, shasum, or openssl)"
  fi
}

verify_checksum() {
  vc_archive="$1"
  vc_checksums="$2"
  vc_expected_name="$3"

  [ -f "$vc_checksums" ] || die "checksum file missing: $vc_checksums"

  vc_expected_hash=$(awk -v name="$vc_expected_name" '
    {
      gsub(/\r$/, "")
      file = $2
      sub(/^\*/, "", file)
      sub(/^\.\//, "", file)
      if (file == name) {
        print tolower($1)
        exit
      }
    }
  ' "$vc_checksums")

  [ -n "$vc_expected_hash" ] || die "archive '$vc_expected_name' not listed in $vc_checksums"

  vc_actual_hash=$(compute_sha256 "$vc_archive" | tr 'A-F' 'a-f')
  if [ "$vc_actual_hash" != "$vc_expected_hash" ]; then
    die "SHA-256 checksum mismatch for $vc_expected_name (expected $vc_expected_hash, got $vc_actual_hash)"
  fi
}

write_marker() {
  wm_root="$1"
  wm_version="$2"
  wm_target="$3"
  wm_bin_dir="$4"
  wm_binaries_space="$5"
  wm_source_mode="$6"

  wm_bins_json=""
  for wm_b in $wm_binaries_space; do
    if [ -z "$wm_bins_json" ]; then
      wm_bins_json="\"$wm_b\""
    else
      wm_bins_json="$wm_bins_json, \"$wm_b\""
    fi
  done

  wm_tmp="$wm_root/$PI_MARKER_NAME.tmp.$$"
  cat >"$wm_tmp" <<EOF
{
  "kind": "pi-rs-managed-install",
  "schemaVersion": 1,
  "layout": "releases-v1",
  "version": "$wm_version",
  "target": "$wm_target",
  "source": "$wm_source_mode",
  "binDir": "$wm_bin_dir",
  "binaries": [$wm_bins_json]
}
EOF
  mv -f "$wm_tmp" "$wm_root/$PI_MARKER_NAME"
}

write_current_version() {
  wcv_root="$1"
  wcv_version="$2"
  wcv_tmp="$wcv_root/current-version.tmp.$$"
  printf '%s\n' "$wcv_version" >"$wcv_tmp"
  mv -f "$wcv_tmp" "$wcv_root/current-version"
}

stage_from_archive() {
  sfa_target="$1"
  sfa_stage_dir="$2"

  command -v tar >/dev/null 2>&1 || die "tar is required to extract release archives"

  if [ -n "$PI_ARCHIVE_PATH" ]; then
    [ -f "$PI_ARCHIVE_PATH" ] || die "archive not found: $PI_ARCHIVE_PATH"
    sfa_archive="$PI_ARCHIVE_PATH"
    sfa_archive_name=$(basename "$sfa_archive")
    if [ "$PI_VERSION" = "latest" ]; then
      # Infer version from pi-rs-<version>-<target>.tar.gz when possible.
      sfa_inferred="${sfa_archive_name#pi-rs-}"
      sfa_inferred="${sfa_inferred%-"${sfa_target}".tar.gz}"
      if [ -n "$sfa_inferred" ] && [ "$sfa_inferred" != "$sfa_archive_name" ]; then
        PI_RESOLVED_VERSION="$sfa_inferred"
      else
        PI_RESOLVED_VERSION="local"
      fi
    else
      PI_RESOLVED_VERSION=$(normalize_version_tag "$PI_VERSION")
    fi
    if [ -n "$PI_CHECKSUMS_PATH" ]; then
      info "Verifying SHA-256 checksum for $sfa_archive_name..."
      verify_checksum "$sfa_archive" "$PI_CHECKSUMS_PATH" "$sfa_archive_name"
    fi
  else
    if [ "$PI_VERSION" = "latest" ]; then
      info "Resolving latest release for ${PI_REPO}..."
      PI_RESOLVED_VERSION=$(resolve_latest_version)
    else
      PI_RESOLVED_VERSION=$(normalize_version_tag "$PI_VERSION")
    fi

    sfa_base_url="${PI_RELEASE_BASE_URL:-https://github.com/${PI_REPO}/releases/download}"
    sfa_base_url="${sfa_base_url%/}"
    sfa_archive_name="pi-rs-${PI_RESOLVED_VERSION}-${sfa_target}.tar.gz"
    sfa_archive="${PI_WORK_DIR}/${sfa_archive_name}"
    sfa_checksums="${PI_WORK_DIR}/SHA256SUMS.txt"

    info "Downloading ${sfa_archive_name} (${PI_RESOLVED_VERSION})..."
    download_file "${sfa_base_url}/${PI_RESOLVED_VERSION}/${sfa_archive_name}" "$sfa_archive" \
      || die "failed to download ${sfa_base_url}/${PI_RESOLVED_VERSION}/${sfa_archive_name}"

    info "Downloading and verifying SHA256SUMS.txt..."
    download_file "${sfa_base_url}/${PI_RESOLVED_VERSION}/SHA256SUMS.txt" "$sfa_checksums" \
      || die "failed to download ${sfa_base_url}/${PI_RESOLVED_VERSION}/SHA256SUMS.txt"

    verify_checksum "$sfa_archive" "$sfa_checksums" "$sfa_archive_name"
  fi

  sfa_extract_dir="${PI_WORK_DIR}/extract"
  mkdir -p "$sfa_extract_dir"
  tar -xzf "$sfa_archive" -C "$sfa_extract_dir"

  sfa_payload_dir="$sfa_extract_dir/pi-rs-${PI_RESOLVED_VERSION}-${sfa_target}"
  if [ ! -d "$sfa_payload_dir" ]; then
    # Fallback: find the single top-level directory or extracted root.
    sfa_first_dir=$(find "$sfa_extract_dir" -mindepth 1 -maxdepth 1 -type d | head -n 1 || true)
    if [ -n "$sfa_first_dir" ] && [ -d "$sfa_first_dir" ]; then
      sfa_payload_dir="$sfa_first_dir"
    else
      sfa_payload_dir="$sfa_extract_dir"
    fi
  fi

  mkdir -p "$sfa_stage_dir/bin"
  sfa_installed_bins=""
  sfa_req_bins=$(resolve_requested_binaries)

  if [ -n "$sfa_req_bins" ]; then
    for sfa_bin in $sfa_req_bins; do
      [ -f "$sfa_payload_dir/$sfa_bin" ] || die "requested binary '$sfa_bin' was not found in archive"
      cp "$sfa_payload_dir/$sfa_bin" "$sfa_stage_dir/bin/$sfa_bin"
      chmod 0755 "$sfa_stage_dir/bin/$sfa_bin"
      sfa_installed_bins="${sfa_installed_bins:+$sfa_installed_bins }$sfa_bin"
    done
  else
    for sfa_bin in $PI_ALL_BINARIES; do
      if [ -f "$sfa_payload_dir/$sfa_bin" ]; then
        cp "$sfa_payload_dir/$sfa_bin" "$sfa_stage_dir/bin/$sfa_bin"
        chmod 0755 "$sfa_stage_dir/bin/$sfa_bin"
        sfa_installed_bins="${sfa_installed_bins:+$sfa_installed_bins }$sfa_bin"
      fi
    done
  fi

  [ -n "$sfa_installed_bins" ] || die "no pi-rs binaries found inside release archive"

  for sfa_doc in LICENSE README.md; do
    if [ -f "$sfa_payload_dir/$sfa_doc" ]; then
      cp "$sfa_payload_dir/$sfa_doc" "$sfa_stage_dir/$sfa_doc"
    fi
  done

  PI_STAGED_BINARIES="$sfa_installed_bins"
}

stage_from_source() {
  sfs_stage_dir="$1"

  command -v cargo >/dev/null 2>&1 || die "--from-source requires Rust and cargo (https://rustup.rs)"

  # Detect whether we are already inside a pi-rs checkout.
  sfs_src_dir=""
  if [ -f "./Cargo.toml" ] && [ -d "./crates/pi-rs-coding-agent" ] && [ -z "$PI_GIT_REF" ] && [ "$PI_VERSION" = "latest" ]; then
    sfs_src_dir=$(pwd)
    info "Building pi-rs from local checkout: $sfs_src_dir"
  else
    command -v git >/dev/null 2>&1 || die "--from-source requires git when run outside a local pi-rs checkout"
    sfs_src_dir="${PI_WORK_DIR}/src"
    sfs_ref="$PI_GIT_REF"
    if [ -z "$sfs_ref" ] && [ "$PI_VERSION" != "latest" ]; then
      sfs_ref=$(normalize_version_tag "$PI_VERSION")
    fi
    info "Cloning https://github.com/${PI_REPO}.git..."
    if [ -n "$sfs_ref" ]; then
      git clone --depth 1 --branch "$sfs_ref" "https://github.com/${PI_REPO}.git" "$sfs_src_dir" \
        || {
          git clone --depth 1 "https://github.com/${PI_REPO}.git" "$sfs_src_dir"
          (cd "$sfs_src_dir" && git fetch --depth 1 origin "$sfs_ref" && git checkout FETCH_HEAD)
        }
    else
      git clone --depth 1 "https://github.com/${PI_REPO}.git" "$sfs_src_dir"
    fi
  fi

  sfs_rev="source"
  if command -v git >/dev/null 2>&1 && [ -d "$sfs_src_dir/.git" ]; then
    sfs_sha=$(git -C "$sfs_src_dir" rev-parse --short HEAD 2>/dev/null || true)
    [ -n "$sfs_sha" ] && sfs_rev="source-${sfs_sha}"
  fi
  if [ "$PI_VERSION" != "latest" ]; then
    PI_RESOLVED_VERSION=$(normalize_version_tag "$PI_VERSION")
  else
    PI_RESOLVED_VERSION="$sfs_rev"
  fi

  sfs_req_bins=$(resolve_requested_binaries)
  if [ -z "$sfs_req_bins" ]; then
    sfs_req_bins="pi pi-ai pi-env pi-evals p3 p2 pi-voice-server"
    if [ "$(uname -s)" = "Darwin" ]; then
      sfs_req_bins="$sfs_req_bins pi-mac"
    fi
  fi

  sfs_target_dir="${CARGO_TARGET_DIR:-$sfs_src_dir/target}"
  sfs_cargo_bins=""
  sfs_need_p2=0

  for sfs_bin in $sfs_req_bins; do
    case "$sfs_bin" in
      pi) sfs_cargo_bins="$sfs_cargo_bins -p pi-rs-coding-agent --bin pi" ;;
      pi-ai) sfs_cargo_bins="$sfs_cargo_bins -p pi-rs-ai --bin pi-ai" ;;
      pi-env) sfs_cargo_bins="$sfs_cargo_bins -p pi-rs-env --bin pi-env" ;;
      pi-evals) sfs_cargo_bins="$sfs_cargo_bins -p pi-rs-evals --bin pi-evals" ;;
      p3) sfs_cargo_bins="$sfs_cargo_bins -p pi-rs-p3 --bin p3" ;;
      pi-voice-server) sfs_cargo_bins="$sfs_cargo_bins -p pi-rs-voice-server --bin pi-voice-server" ;;
      pi-mac) sfs_cargo_bins="$sfs_cargo_bins -p pi-rs-p2-mac --bin pi-mac" ;;
      p2) sfs_need_p2=1 ;;
    esac
  done

  if [ -n "$sfs_cargo_bins" ]; then
    info "Compiling release binaries with cargo..."
    # shellcheck disable=SC2086
    (cd "$sfs_src_dir" && cargo build --release --locked $sfs_cargo_bins)
  fi

  if [ "$sfs_need_p2" -eq 1 ]; then
    info "Building packaged p2 binary (with Leptos WASM browser UI)..."
    (cd "$sfs_src_dir" && ./scripts/package-p2.sh --release --locked >/dev/null)
  fi

  mkdir -p "$sfs_stage_dir/bin"
  sfs_installed_bins=""
  for sfs_bin in $sfs_req_bins; do
    sfs_built="${sfs_target_dir%/}/release/${sfs_bin}"
    [ -f "$sfs_built" ] || die "expected built binary missing: $sfs_built"
    cp "$sfs_built" "$sfs_stage_dir/bin/$sfs_bin"
    chmod 0755 "$sfs_stage_dir/bin/$sfs_bin"
    sfs_installed_bins="${sfs_installed_bins:+$sfs_installed_bins }$sfs_bin"
  done

  for sfs_doc in LICENSE README.md; do
    if [ -f "$sfs_src_dir/$sfs_doc" ]; then
      cp "$sfs_src_dir/$sfs_doc" "$sfs_stage_dir/$sfs_doc"
    fi
  done

  PI_STAGED_BINARIES="$sfs_installed_bins"
}

verify_staged_release() {
  vsr_stage_dir="$1"
  if [ -x "$vsr_stage_dir/bin/pi" ]; then
    vsr_out=$("$vsr_stage_dir/bin/pi" --version 2>&1) || die "smoke test failed for staged pi binary: $vsr_out"
    [ -n "$vsr_out" ] || die "staged pi --version returned empty output"
  fi
}

cleanup_old_releases() {
  cor_releases_dir="$1"
  cor_keep_version="$2"
  [ -d "$cor_releases_dir" ] || return 0
  for cor_entry in "$cor_releases_dir"/*; do
    [ -e "$cor_entry" ] || continue
    cor_name=$(basename "$cor_entry")
    if [ "$cor_name" != "$cor_keep_version" ]; then
      rm -rf "$cor_entry"
    fi
  done
}

install_release() {
  ir_install_dir="$PI_RESOLVED_INSTALL_DIR"
  ir_bin_dir="$PI_RESOLVED_BIN_DIR"
  ir_source_mode="release"

  if [ "$PI_OPT_FROM_SOURCE" -eq 1 ]; then
    ir_source_mode="source"
    ir_target="$(uname -m)-$(uname -s)"
  else
    ir_target=$(detect_target)
  fi

  mkdir -p "$ir_install_dir/staging" "$ir_install_dir/releases" "$ir_bin_dir"
  ir_stage_dir="$ir_install_dir/staging/install-$$"
  rm -rf "$ir_stage_dir"
  mkdir -p "$ir_stage_dir"

  if [ "$PI_OPT_FROM_SOURCE" -eq 1 ]; then
    stage_from_source "$ir_stage_dir"
  else
    stage_from_archive "$ir_target" "$ir_stage_dir"
  fi

  verify_staged_release "$ir_stage_dir"

  ir_release_dir="$ir_install_dir/releases/$PI_RESOLVED_VERSION"
  rm -rf "$ir_release_dir"
  mv "$ir_stage_dir" "$ir_release_dir"

  # Update `current` symlink (use ln -sfn directly; mv into an existing symlink-to-dir
  # moves the source inside the old target directory instead of replacing the symlink).
  ln -sfn "$ir_release_dir" "$ir_install_dir/current"
  write_current_version "$ir_install_dir" "$PI_RESOLVED_VERSION"

  # Remove any stale symlinks in ir_bin_dir that belonged to a previous managed release
  # of binaries no longer selected.
  for ir_old_bin in $PI_ALL_BINARIES; do
    if [ -L "$ir_bin_dir/$ir_old_bin" ] && command_belongs_to_managed_root "$ir_bin_dir/$ir_old_bin" "$ir_install_dir"; then
      case " $PI_STAGED_BINARIES " in
        *" $ir_old_bin "*) ;;
        *) rm -f "$ir_bin_dir/$ir_old_bin" ;;
      esac
    fi
  done

  # Link newly installed binaries into ir_bin_dir.
  for ir_bin in $PI_STAGED_BINARIES; do
    if [ -e "$ir_bin_dir/$ir_bin" ] && ! command_belongs_to_managed_root "$ir_bin_dir/$ir_bin" "$ir_install_dir"; then
      if [ "$PI_OPT_FORCE" -eq 0 ]; then
        die "refusing to overwrite existing non-managed binary at $ir_bin_dir/$ir_bin (use --bin-dir or --force)"
      fi
    fi
    ir_link_tmp="$ir_bin_dir/.${ir_bin}.tmp.$$"
    ln -sfn "$ir_install_dir/current/bin/$ir_bin" "$ir_link_tmp"
    mv -f "$ir_link_tmp" "$ir_bin_dir/$ir_bin"
  done

  write_marker "$ir_install_dir" "$PI_RESOLVED_VERSION" "$ir_target" "$ir_bin_dir" "$PI_STAGED_BINARIES" "$ir_source_mode"
  cleanup_old_releases "$ir_install_dir/releases" "$PI_RESOLVED_VERSION"
  rm -rf "$ir_install_dir/staging"
}

uninstall_release() {
  ur_install_dir="$PI_RESOLVED_INSTALL_DIR"
  ur_bin_dir="$PI_RESOLVED_BIN_DIR"

  if [ ! -d "$ur_install_dir" ]; then
    info "No managed pi-rs installation found at $ur_install_dir."
    return 0
  fi

  # Collect candidate bin dirs to clean managed symlinks from.
  ur_recorded_bin=$(marker_read_field "$ur_install_dir/$PI_MARKER_NAME" "binDir" || true)
  ur_dirs="$ur_bin_dir"
  if [ -n "$ur_recorded_bin" ] && [ "$ur_recorded_bin" != "$ur_bin_dir" ]; then
    ur_dirs="$ur_dirs:$ur_recorded_bin"
  fi

  ur_old_ifs=${IFS- }
  IFS=:
  for ur_dir in $ur_dirs; do
    IFS=$ur_old_ifs
    [ -n "$ur_dir" ] && [ -d "$ur_dir" ] || { IFS=:; continue; }
    for ur_bin in $PI_ALL_BINARIES; do
      if [ -L "$ur_dir/$ur_bin" ] && command_belongs_to_managed_root "$ur_dir/$ur_bin" "$ur_install_dir"; then
        rm -f "$ur_dir/$ur_bin"
      fi
    done
    IFS=:
  done
  IFS=$ur_old_ifs

  rm -rf "$ur_install_dir"
  info "pi-rs was uninstalled successfully."
  info "Your configuration, credentials, sessions, and packages in ${PI_AGENT_DIR} were preserved."
}

shell_config_file() {
  scf_shell=$(basename "${SHELL:-sh}")
  case "$scf_shell" in
    fish) printf '%s/.config/fish/config.fish' "$HOME" ;;
    zsh) printf '%s/.zshrc' "${ZDOTDIR:-$HOME}" ;;
    bash)
      if [ -f "$HOME/.bashrc" ]; then
        printf '%s/.bashrc' "$HOME"
      else
        printf '%s/.profile' "$HOME"
      fi
      ;;
    *) printf '%s/.profile' "$HOME" ;;
  esac
}

path_update_command() {
  puc_dir="$1"
  puc_shell=$(basename "${SHELL:-sh}")
  if [ "$puc_dir" = "$HOME/.local/bin" ]; then
    puc_expr='$HOME/.local/bin'
  else
    puc_expr="$puc_dir"
  fi
  case "$puc_shell" in
    fish) printf 'fish_add_path "%s"' "$puc_expr" ;;
    *) printf 'export PATH="%s:$PATH"' "$puc_expr" ;;
  esac
}

prompt_add_path_to_profile() {
  pap_dir="$1"
  [ "$PI_OPT_NO_MODIFY_PATH" -eq 0 ] || return 1
  has_tty || return 1

  pap_cfg=$(shell_config_file)
  pap_cmd=$(path_update_command "$pap_dir")

  if [ -f "$pap_cfg" ] && grep -Fxq "$pap_cmd" "$pap_cfg"; then
    printf 'A PATH update for %s already exists in %s.\n' "$pap_dir" "$pap_cfg"
    return 0
  fi

  exec 3<>/dev/tty
  printf 'Add %s to your PATH in %s now? [Y/n] ' "$pap_dir" "$pap_cfg" >&3
  if ! IFS= read -r pap_answer <&3; then
    pap_answer="n"
  fi
  exec 3>&-
  case "$pap_answer" in
    n|N|no|NO) return 1 ;;
  esac

  mkdir -p "$(dirname "$pap_cfg")"
  touch "$pap_cfg"
  printf '\n# pi-rs\n%s\n' "$pap_cmd" >>"$pap_cfg"
  printf 'Added %s to %s.\n' "$pap_dir" "$pap_cfg"
}

installed_pi_is_first_on_path() {
  ip_installed="$PI_RESOLVED_BIN_DIR/$PI_CMD"
  [ -x "$ip_installed" ] || return 1
  ip_active=$(command -v "$PI_CMD" 2>/dev/null || true)
  [ -n "$ip_active" ] || return 1
  [ "$(resolve_path "$ip_active")" = "$(resolve_path "$ip_installed")" ]
}

prompt_start_pi() {
  [ "$PI_OPT_NO_START" -eq 0 ] || return 0
  psp_path="$PI_RESOLVED_BIN_DIR/$PI_CMD"
  [ -x "$psp_path" ] || return 0
  [ -t 1 ] || return 0
  has_tty || return 0

  exec 3<>/dev/tty
  printf '\nStart pi now? [Y/n] ' >&3
  if ! IFS= read -r psp_answer <&3; then
    psp_answer="n"
  fi
  exec 3>&-
  case "$psp_answer" in
    n|N|no|NO) return 0 ;;
  esac

  printf '\n'
  exec "$psp_path" </dev/tty
}

main() {
  parse_args "$@"

  PI_WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/pi-rs-installer.XXXXXX")
  trap 'rm -rf "${PI_WORK_DIR:-}"' EXIT

  PI_RESOLVED_INSTALL_DIR=$(select_install_dir)
  PI_AGENT_DIR=$(dirname "$PI_RESOLVED_INSTALL_DIR")

  PI_EXISTING_PATH=$(command -v "$PI_CMD" 2>/dev/null || true)
  if [ -n "$PI_EXISTING_PATH" ] && path_is_on_windows_drive "$PI_EXISTING_PATH" && ! command_belongs_to_managed_root "$PI_EXISTING_PATH" "$PI_RESOLVED_INSTALL_DIR"; then
    PI_EXISTING_PATH=""
  fi

  PI_HAS_MANAGED=0
  if [ -f "$PI_RESOLVED_INSTALL_DIR/$PI_MARKER_NAME" ] || [ -L "$PI_RESOLVED_INSTALL_DIR/current" ]; then
    PI_HAS_MANAGED=1
  fi

  PI_OTHER_PI_PATH=""
  if [ -n "$PI_EXISTING_PATH" ] && ! command_belongs_to_managed_root "$PI_EXISTING_PATH" "$PI_RESOLVED_INSTALL_DIR"; then
    PI_OTHER_PI_PATH="$PI_EXISTING_PATH"
  fi

  PI_RESOLVED_BIN_DIR=$(select_bin_dir "$PI_AGENT_DIR" "$PI_RESOLVED_INSTALL_DIR" "$PI_EXISTING_PATH")

  choose_action

  case "$PI_ACTION" in
    none)
      info "Cancelled."
      exit 0
      ;;
    uninstall)
      uninstall_release
      exit 0
      ;;
    install|reinstall)
      install_release
      ;;
    *)
      die "unexpected installer action: $PI_ACTION"
      ;;
  esac

  if [ "$PI_ACTION" = "reinstall" ]; then
    printf '\npi-rs (%s) was updated/reinstalled successfully.\n' "$PI_RESOLVED_VERSION"
  else
    printf '\npi-rs (%s) was installed successfully.\n' "$PI_RESOLVED_VERSION"
  fi
  printf 'Installed binaries (%s): %s\n' "$PI_RESOLVED_BIN_DIR" "$PI_STAGED_BINARIES"
  printf 'Update or uninstall later by re-running the installer.\n'

  if [ -x "$PI_RESOLVED_BIN_DIR/$PI_CMD" ]; then
    if installed_pi_is_first_on_path; then
      printf '\nRun it with: pi\n'
    else
      printf '\npi-rs was installed, but your current shell is not resolving %s/pi first on PATH.\n' "$PI_RESOLVED_BIN_DIR"
      if [ -n "$PI_OTHER_PI_PATH" ]; then
        printf 'Your shell currently resolves pi to: %s\n' "$PI_OTHER_PI_PATH"
      fi
      prompt_add_path_to_profile "$PI_RESOLVED_BIN_DIR" || true
      printf 'Restart your shell or run:\n\n  %s\n\nThen run: pi\n' "$(path_update_command "$PI_RESOLVED_BIN_DIR")"
    fi
    prompt_start_pi
  fi
}

main "$@"
