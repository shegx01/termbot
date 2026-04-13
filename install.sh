#!/usr/bin/env bash
# termbot installer — install, upgrade, or uninstall termbot
# https://github.com/shegx01/termbot
#
# Usage:
#   curl -sSL https://raw.githubusercontent.com/shegx01/termbot/main/install.sh | bash
#   bash install.sh [--quick] [--upgrade] [--uninstall]
#
# The entire script is wrapped in main() so that a partial download via
# curl|bash never executes an incomplete script.

set -euo pipefail

# =============================================================================
# Constants
# =============================================================================

REPO="shegx01/termbot"
GITHUB_RELEASES="https://api.github.com/repos/${REPO}/releases/latest"
BINARY_NAME="termbot"
INSTALL_DIR="${HOME}/.local/bin"
CONFIG_DIR="${HOME}/.config/termbot"
CONFIG_FILE="${CONFIG_DIR}/termbot.toml"
VERSION_FILE="${CONFIG_DIR}/.installed-version"
LOG_DIR="${CONFIG_DIR}/logs"
BINARY_PATH="${INSTALL_DIR}/${BINARY_NAME}"
BACKUP_PATH="${INSTALL_DIR}/${BINARY_NAME}.bak"
UPDATE_CHECK_PATH="${INSTALL_DIR}/termbot-update-check"
WRAPPER_PATH="${INSTALL_DIR}/termbot-wrapper"

# macOS paths
LAUNCHD_SERVICE="${HOME}/Library/LaunchAgents/com.termbot.agent.plist"
LAUNCHD_UPDATE="${HOME}/Library/LaunchAgents/com.termbot.update-check.plist"

# Linux paths
SYSTEMD_DIR="${HOME}/.config/systemd/user"
SYSTEMD_SERVICE="${SYSTEMD_DIR}/termbot.service"
SYSTEMD_UPDATE_SERVICE="${SYSTEMD_DIR}/termbot-update-check.service"
SYSTEMD_UPDATE_TIMER="${SYSTEMD_DIR}/termbot-update-check.timer"

# Cached release JSON (fetched once, reused for version + checksum)
_RELEASE_JSON=""

# Temp file path (for cleanup trap)
_TMP_FILE=""

# Version downloaded (set by download_binary, written by caller after verify)
_DOWNLOADED_VERSION=""

# =============================================================================
# Global state (set by interactive prompts)
# =============================================================================

PLATFORM="" # telegram | slack | both
TG_BOT_TOKEN=""
TG_USER_ID=""
SL_BOT_TOKEN=""
SL_APP_TOKEN=""
SL_USER_ID=""
SL_CHANNEL_ID=""

# Flags
QUICK=false
UPGRADE=false
UNINSTALL=false

# =============================================================================
# Colors + glyphs (disabled when stderr is not a terminal)
# =============================================================================

setup_colors() {
  if [ -t 2 ]; then
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    YELLOW='\033[1;33m'
    BLUE='\033[0;34m'
    CYAN='\033[0;36m'
    BOLD='\033[1m'
    DIM='\033[2m'
    NC='\033[0m'
    # Use fancy glyphs only in UTF-8 locales
    case "${LANG:-}${LC_ALL:-}${LC_CTYPE:-}" in
      *[Uu][Tt][Ff][-_]8*)
        TICK='✓'
        CROSS='✗'
        ARROW='▸'
        ;;
      *)
        TICK='+'
        CROSS='x'
        ARROW='>'
        ;;
    esac
  else
    RED='' GREEN='' YELLOW='' BLUE='' CYAN='' BOLD='' DIM='' NC=''
    TICK='+' CROSS='x' ARROW='>'
  fi
}

# =============================================================================
# Cleanup trap — removes temp files on interrupt / error
# =============================================================================

cleanup() {
  if [ -n "${_TMP_FILE:-}" ] && [ -f "$_TMP_FILE" ]; then
    rm -f "$_TMP_FILE"
  fi
}

# =============================================================================
# Output helpers (all go to stderr to keep stdout clean)
# =============================================================================

info() { printf "  %b%s%b %s\n" "$GREEN" "$TICK" "$NC" "$1" >&2; }
warn() { printf "  %b!%b %s\n" "$YELLOW" "$NC" "$1" >&2; }
error() { printf "  %b%s%b %s\n" "$RED" "$CROSS" "$NC" "$1" >&2; }
step() { printf "\n%b%b%s %s%b\n" "$BOLD" "$BLUE" "$ARROW" "$1" "$NC" >&2; }
hint() { if [ "$QUICK" = false ]; then printf "    %b%s%b\n" "$DIM" "$1" "$NC" >&2; fi; }
hint_always() { printf "    %b%s%b\n" "$DIM" "$1" "$NC" >&2; }

# =============================================================================
# Input helpers (read from /dev/tty so curl|bash works)
# =============================================================================

prompt() {
  local message="$1"
  local default="${2:-}"
  local result

  if [ -n "$default" ]; then
    printf "  %b%s%b [%s]: " "$BOLD" "$message" "$NC" "$default" >&2
  else
    printf "  %b%s%b: " "$BOLD" "$message" "$NC" >&2
  fi

  read -r result </dev/tty || true

  # Strip leading/trailing whitespace (common paste artifacts)
  result="$(printf '%s' "$result" | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')"

  if [ -z "$result" ] && [ -n "$default" ]; then
    result="$default"
  fi

  printf "%s" "$result"
}

prompt_choice() {
  local message="$1"
  shift
  local i=1
  local choice

  printf "\n  %b%s%b\n" "$BOLD" "$message" "$NC" >&2
  for opt in "$@"; do
    printf "    %b%d)%b %s\n" "$CYAN" "$i" "$NC" "$opt" >&2
    i=$((i + 1))
  done
  printf "  %b>%b " "$BOLD" "$NC" >&2

  read -r choice </dev/tty || true
  printf "%s" "$choice"
}

prompt_yn() {
  local message="$1"
  local default="${2:-y}"
  local result

  while true; do
    if [ "$default" = "y" ]; then
      printf "  %b%s%b [Y/n]: " "$BOLD" "$message" "$NC" >&2
    else
      printf "  %b%s%b [y/N]: " "$BOLD" "$message" "$NC" >&2
    fi

    read -r result </dev/tty || true
    result="${result:-$default}"
    result="$(printf "%s" "$result" | tr '[:upper:]' '[:lower:]')"

    case "$result" in
      y | yes) return 0 ;;
      n | no) return 1 ;;
      *) warn "Please answer y or n." ;;
    esac
  done
}

# =============================================================================
# Safety helpers
# =============================================================================

# Check that /dev/tty is available for interactive prompts
require_tty() {
  if [ ! -c /dev/tty ]; then
    error "This installer requires an interactive terminal."
    hint_always "When using SSH: ssh -t user@host 'curl ... | bash'"
    hint_always "Or download and run directly: bash install.sh"
    exit 1
  fi
}

# Check if systemd is available and functional for user units
has_systemd() {
  command -v systemctl >/dev/null 2>&1 \
    && systemctl --user show-environment >/dev/null 2>&1
}

# Ensure DBUS session bus is set (required for systemctl --user over SSH)
ensure_user_bus() {
  if [ -z "${XDG_RUNTIME_DIR:-}" ]; then
    local uid
    uid="$(id -u)"
    export XDG_RUNTIME_DIR="/run/user/${uid}"
  fi
  if [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ] && [ -S "${XDG_RUNTIME_DIR}/bus" ]; then
    export DBUS_SESSION_BUS_ADDRESS="unix:path=${XDG_RUNTIME_DIR}/bus"
  fi
}

# Sanitize a value for safe embedding in a TOML double-quoted string.
# Rejects control characters, escapes backslashes and double-quotes.
sanitize_toml_value() {
  local val="$1"
  # Reject control characters (newlines, tabs, carriage returns)
  # Note: $'\n' is used instead of $(printf '\n') because command
  # substitution strips trailing newlines, producing an empty match.
  case "$val" in
    *$'\n'* | *$'\r'* | *$'\t'*)
      error "Input contains invalid characters (control characters not allowed)."
      exit 1
      ;;
  esac
  # Escape backslashes then double-quotes
  val="$(printf '%s' "$val" | sed 's/\\/\\\\/g; s/"/\\"/g')"
  printf '%s' "$val"
}

# Escape a string for safe embedding in an XML <string> element (launchd plists)
xml_escape() {
  local s="$1"
  s="$(printf '%s' "$s" | sed 's/&/\&amp;/g; s/</\&lt;/g; s/>/\&gt;/g; s/"/\&quot;/g')"
  printf '%s' "$s"
}

# =============================================================================
# OS-native notifications (safe from injection)
# =============================================================================

notify_os() {
  local title="$1"
  local message="$2"

  case "$(uname -s)" in
    Darwin)
      # Use argv-based AppleScript to avoid string injection
      osascript \
        -e 'on run argv' \
        -e 'display notification (item 2 of argv) with title (item 1 of argv)' \
        -e 'end run' \
        -- "$title" "$message" 2>/dev/null || true
      ;;
    Linux)
      if command -v notify-send >/dev/null 2>&1; then
        notify-send "$title" "$message" 2>/dev/null || true
      fi
      ;;
  esac
}

# =============================================================================
# Platform detection
# =============================================================================

detect_os() {
  case "$(uname -s)" in
    Darwin) printf "macos" ;;
    Linux) printf "linux" ;;
    *)
      error "Unsupported OS: $(uname -s). termbot supports macOS and Linux."
      exit 1
      ;;
  esac
}

detect_arch() {
  case "$(uname -m)" in
    x86_64 | amd64) printf "x86_64" ;;
    aarch64 | arm64) printf "aarch64" ;;
    *)
      error "Unsupported architecture: $(uname -m)"
      exit 1
      ;;
  esac
}

get_target() {
  local os arch
  os="$(detect_os)"
  arch="$(detect_arch)"

  case "${os}-${arch}" in
    macos-x86_64) printf "x86_64-apple-darwin" ;;
    macos-aarch64) printf "aarch64-apple-darwin" ;;
    linux-x86_64) printf "x86_64-unknown-linux-gnu" ;;
    linux-aarch64) printf "aarch64-unknown-linux-gnu" ;;
    *)
      error "No pre-built binary for ${os} ${arch}"
      exit 1
      ;;
  esac
}

detect_package_manager() {
  if command -v brew >/dev/null 2>&1; then
    printf "brew"
  elif command -v apt-get >/dev/null 2>&1; then
    printf "apt"
  elif command -v dnf >/dev/null 2>&1; then
    printf "dnf"
  elif command -v yum >/dev/null 2>&1; then
    printf "yum"
  elif command -v pacman >/dev/null 2>&1; then
    printf "pacman"
  else
    printf "unknown"
  fi
}

# =============================================================================
# Dependency checks
# =============================================================================

check_curl() {
  if ! command -v curl >/dev/null 2>&1; then
    error "curl is required but not found. Please install curl first."
    exit 1
  fi
  info "curl found"
}

check_tmux() {
  if command -v tmux >/dev/null 2>&1; then
    info "tmux found: $(tmux -V)"
    return 0
  fi

  warn "tmux is required but not found."

  local pm
  pm="$(detect_package_manager)"

  if [ "$pm" = "unknown" ]; then
    error "Could not detect a package manager. Please install tmux manually and re-run."
    exit 1
  fi

  if prompt_yn "Install tmux via ${pm}?"; then
    install_tmux "$pm"
  else
    error "tmux is required. Install it and try again."
    exit 1
  fi
}

install_tmux() {
  local pm="$1"
  step "Installing tmux via ${pm}"

  local install_ok=true
  case "$pm" in
    brew) brew install tmux >&2 || install_ok=false ;;
    apt) (sudo apt-get update -qq >&2 && sudo apt-get install -y tmux >&2) || install_ok=false ;;
    dnf) sudo dnf install -y tmux >&2 || install_ok=false ;;
    yum) sudo yum install -y tmux >&2 || install_ok=false ;;
    pacman) sudo pacman -S --noconfirm tmux >&2 || install_ok=false ;;
  esac

  if command -v tmux >/dev/null 2>&1; then
    info "tmux installed: $(tmux -V)"
  else
    error "tmux installation failed. Install it manually and re-run."
    exit 1
  fi
}

check_claude() {
  if command -v claude >/dev/null 2>&1; then
    info "Claude Code CLI found"
    return 0
  fi

  error "Claude Code CLI is required but not found."
  hint_always "Install it with: npm i -g @anthropic-ai/claude-code"
  hint_always "Then authenticate: claude login"
  exit 1
}

# =============================================================================
# Token / ID validation (strict character sets to prevent injection)
# =============================================================================

validate_telegram_token() {
  # Real format: digits:alphanumeric-underscore  e.g. 7012345678:AAHxyz_abc-123
  case "$1" in
    *[!0-9A-Za-z:_-]*) return 1 ;; # reject chars outside allowed set
    [0-9]*:*) return 0 ;;          # must start with digits, contain colon
    *) return 1 ;;
  esac
}

validate_telegram_user_id() {
  case "$1" in
    '' | *[!0-9]*) return 1 ;;
    *) return 0 ;;
  esac
}

validate_slack_bot_token() {
  case "$1" in
    *[!0-9A-Za-z_-]*) return 1 ;; # reject non-token chars
    xoxb-*) return 0 ;;
    *) return 1 ;;
  esac
}

validate_slack_app_token() {
  case "$1" in
    *[!0-9A-Za-z_-]*) return 1 ;;
    xapp-*) return 0 ;;
    *) return 1 ;;
  esac
}

validate_slack_user_id() {
  case "$1" in
    U[A-Z0-9]*) return 0 ;;
    *) return 1 ;;
  esac
}

validate_slack_channel_id() {
  case "$1" in
    C[A-Z0-9]*) return 0 ;;
    *) return 1 ;;
  esac
}

# =============================================================================
# Interactive configuration
# =============================================================================

prompt_platform_choice() {
  while true; do
    local choice
    choice="$(prompt_choice "Which platform will you use?" "Telegram" "Slack" "Both")"

    case "$choice" in
      1)
        PLATFORM="telegram"
        return
        ;;
      2)
        PLATFORM="slack"
        return
        ;;
      3)
        PLATFORM="both"
        return
        ;;
      *) warn "Invalid choice -- enter 1, 2, or 3." ;;
    esac
  done
}

prompt_telegram() {
  step "Telegram configuration"

  hint "You'll need your bot token and user ID."
  hint ""

  # Bot token
  while true; do
    hint "To get a bot token:"
    hint "  1. Message @BotFather on Telegram"
    hint "  2. Send /newbot and follow the prompts"
    hint "  3. Copy the token (looks like 7012345678:AAH...)"
    hint ""

    TG_BOT_TOKEN="$(prompt "Telegram bot token")"

    if validate_telegram_token "$TG_BOT_TOKEN"; then
      info "Token format valid"
      break
    else
      warn "That doesn't look like a Telegram token (expected: 1234567890:AAHx...)"
    fi
  done

  # User ID
  while true; do
    hint ""
    hint "To get your user ID:"
    hint "  1. Message @userinfobot on Telegram"
    hint "  2. It replies with your numeric ID"
    hint ""

    TG_USER_ID="$(prompt "Your Telegram user ID")"

    if validate_telegram_user_id "$TG_USER_ID"; then
      info "User ID format valid"
      break
    else
      warn "User ID should be a number (e.g., 123456789)"
    fi
  done
}

prompt_slack() {
  step "Slack configuration"

  hint "You'll need tokens from your Slack app."
  hint "Create one at: https://api.slack.com/apps"
  hint ""

  # Bot token
  while true; do
    hint "Bot token: OAuth & Permissions -> Bot User OAuth Token"
    hint "  Required scopes: chat:write, channels:history, channels:read"
    hint ""

    SL_BOT_TOKEN="$(prompt "Slack bot token (xoxb-...)")"

    if validate_slack_bot_token "$SL_BOT_TOKEN"; then
      info "Bot token format valid"
      break
    else
      warn "Slack bot token should start with xoxb- and contain only alphanumeric characters"
    fi
  done

  # App token
  while true; do
    hint ""
    hint "App token: Settings -> Socket Mode -> Generate token"
    hint "  Required scope: connections:write"
    hint ""

    SL_APP_TOKEN="$(prompt "Slack app token (xapp-...)")"

    if validate_slack_app_token "$SL_APP_TOKEN"; then
      info "App token format valid"
      break
    else
      warn "Slack app token should start with xapp- and contain only alphanumeric characters"
    fi
  done

  # Member ID
  while true; do
    hint ""
    hint "Member ID: Click your profile picture -> Profile -> ... -> Copy member ID"
    hint ""

    SL_USER_ID="$(prompt "Your Slack member ID (U...)")"

    if validate_slack_user_id "$SL_USER_ID"; then
      info "Member ID format valid"
      break
    else
      warn "Slack member ID should start with U followed by alphanumeric characters"
    fi
  done

  # Channel ID
  while true; do
    hint ""
    hint "Channel ID: Right-click channel -> View channel details -> ID at bottom"
    hint ""

    SL_CHANNEL_ID="$(prompt "Slack channel ID (C...)")"

    if validate_slack_channel_id "$SL_CHANNEL_ID"; then
      info "Channel ID format valid"
      break
    else
      warn "Slack channel ID should start with C followed by alphanumeric characters"
    fi
  done
}

# =============================================================================
# Config file writer (with TOML injection prevention + restrictive permissions)
# =============================================================================

write_config() {
  # Set restrictive umask so both directory and file are created securely
  local old_umask
  old_umask="$(umask)"
  umask 077

  mkdir -p "$CONFIG_DIR"

  {
    printf "# termbot configuration\n"
    printf "# Generated by install.sh on %s\n" "$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
    printf "# Docs: https://github.com/%s#configuration\n\n" "$REPO"

    # [auth]
    printf "[auth]\n"
    if [ "$PLATFORM" = "telegram" ] || [ "$PLATFORM" = "both" ]; then
      printf "telegram_user_id = %s\n" "$TG_USER_ID"
    fi
    if [ "$PLATFORM" = "slack" ] || [ "$PLATFORM" = "both" ]; then
      printf "slack_user_id = \"%s\"\n" "$(sanitize_toml_value "$SL_USER_ID")"
    fi

    # [telegram]
    if [ "$PLATFORM" = "telegram" ] || [ "$PLATFORM" = "both" ]; then
      printf "\n[telegram]\n"
      printf "bot_token = \"%s\"\n" "$(sanitize_toml_value "$TG_BOT_TOKEN")"
    fi

    # [slack]
    if [ "$PLATFORM" = "slack" ] || [ "$PLATFORM" = "both" ]; then
      printf "\n[slack]\n"
      printf "bot_token = \"%s\"\n" "$(sanitize_toml_value "$SL_BOT_TOKEN")"
      printf "app_token = \"%s\"\n" "$(sanitize_toml_value "$SL_APP_TOKEN")"
      printf "channel_id = \"%s\"\n" "$(sanitize_toml_value "$SL_CHANNEL_ID")"
    fi

    # [blocklist]
    cat <<'BLOCKLIST'

[blocklist]
patterns = [
  "rm\\s+-[a-z]*f[a-z]*r[a-z]*\\s+/",
  "sudo\\s+",
  ":\\(\\)\\{\\s*:\\|:\\&\\s*\\};:",
  "mkfs\\.",
  "dd\\s+if=",
]
BLOCKLIST

    # [streaming]
    cat <<'STREAMING'

[streaming]
edit_throttle_ms = 2000
poll_interval_ms = 250
chunk_size = 4000
offline_buffer_max_bytes = 1048576
max_sessions = 10
STREAMING
  } >"$CONFIG_FILE"

  chmod 600 "$CONFIG_FILE"
  umask "$old_umask"
  info "Config written to ${CONFIG_FILE}"
}

# =============================================================================
# Release info (fetched once, cached)
# =============================================================================

fetch_release_info() {
  # NOTE: callers must populate _RELEASE_JSON in the parent shell before
  # calling get_latest_version / get_expected_checksum, because $() runs
  # in a subshell and assignments don't propagate back.
  printf '%s' "$_RELEASE_JSON"
}

# Fetch the release JSON once into the global cache.
# Call this in the parent shell (not inside $()) before using version/checksum helpers.
prefetch_release_info() {
  if [ -z "$_RELEASE_JSON" ]; then
    _RELEASE_JSON="$(curl -fsSL --connect-timeout 10 --max-time 30 \
      "$GITHUB_RELEASES" 2>/dev/null || printf "")"
    # Validate response is actual JSON
    if [ -n "$_RELEASE_JSON" ] && ! printf '%s' "$_RELEASE_JSON" | grep -q '"tag_name"'; then
      warn "GitHub API returned unexpected response (rate-limited?)."
      _RELEASE_JSON=""
    fi
  fi
}

get_latest_version() {
  local json
  json="$(fetch_release_info)"
  if [ -n "$json" ]; then
    printf '%s' "$json" | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/'
  else
    printf "unknown"
  fi
}

# Extract the expected SHA-256 checksum for a given binary from the release body
get_expected_checksum() {
  local target="$1" json
  json="$(fetch_release_info)"
  if [ -z "$json" ]; then
    return 1
  fi
  # Release body contains lines like:  <hash>  termbot-<target>
  printf '%s' "$json" | grep -oE '[a-f0-9]{64}  termbot-'"${target}" | head -1 | awk '{print $1}'
}

# =============================================================================
# Binary download (with checksum verification + atomic swap)
# =============================================================================

download_binary() {
  local target="$1"

  step "Downloading termbot"

  # Populate the release cache in the parent shell (avoids subshell leak)
  prefetch_release_info

  local version
  version="$(get_latest_version)"
  if [ "$version" = "unknown" ]; then
    warn "Could not determine latest version from GitHub API."
    warn "Falling back to /releases/latest redirect."
  fi

  local url
  if [ "$version" != "unknown" ]; then
    url="https://github.com/${REPO}/releases/download/${version}/${BINARY_NAME}-${target}"
  else
    url="https://github.com/${REPO}/releases/latest/download/${BINARY_NAME}-${target}"
  fi

  hint_always "Target: ${target}"
  hint_always "URL:    ${url}"

  if ! mkdir -p "$INSTALL_DIR"; then
    error "Could not create install directory: ${INSTALL_DIR}"
    exit 1
  fi

  _TMP_FILE="$(mktemp "${INSTALL_DIR}/${BINARY_NAME}.XXXXXX")" || {
    error "Failed to create temporary file in ${INSTALL_DIR}."
    exit 1
  }

  if ! curl -fSL --connect-timeout 15 --max-time 300 --progress-bar -o "$_TMP_FILE" "$url" 2>&1; then
    rm -f "$_TMP_FILE"
    _TMP_FILE=""
    error "Download failed. Check your internet connection and try again."
    exit 1
  fi

  # Verify download is non-empty
  if [ ! -s "$_TMP_FILE" ]; then
    rm -f "$_TMP_FILE"
    _TMP_FILE=""
    error "Downloaded file is empty (disk full?)."
    exit 1
  fi

  # Checksum verification (best-effort: warns if checksums unavailable)
  local expected_checksum actual_checksum
  expected_checksum="$(get_expected_checksum "$target" || printf "")"

  if [ -n "$expected_checksum" ]; then
    if command -v sha256sum >/dev/null 2>&1; then
      actual_checksum="$(sha256sum "$_TMP_FILE" | awk '{print $1}')"
    elif command -v shasum >/dev/null 2>&1; then
      actual_checksum="$(shasum -a 256 "$_TMP_FILE" | awk '{print $1}')"
    fi

    if [ -n "$actual_checksum" ]; then
      if [ "$expected_checksum" = "$actual_checksum" ]; then
        info "Checksum verified (SHA-256)"
      else
        rm -f "$_TMP_FILE"
        _TMP_FILE=""
        error "Checksum verification FAILED. The download may be corrupted or tampered with."
        error "Expected: ${expected_checksum}"
        error "Got:      ${actual_checksum}"
        exit 1
      fi
    else
      warn "No SHA-256 tool available. Skipping checksum verification."
    fi
  else
    warn "Checksums not available from release. Skipping verification."
  fi

  if ! chmod +x "$_TMP_FILE"; then
    rm -f "$_TMP_FILE"
    _TMP_FILE=""
    error "Failed to set executable permission on downloaded binary."
    exit 1
  fi

  # Atomic swap — cp keeps original in place until the final mv
  if [ -f "$BINARY_PATH" ]; then
    if ! cp "$BINARY_PATH" "$BACKUP_PATH"; then
      error "Failed to back up existing binary. Check disk space and permissions."
      rm -f "$_TMP_FILE"
      _TMP_FILE=""
      exit 1
    fi
    info "Previous binary backed up to ${BACKUP_PATH}"
  fi

  if ! mv "$_TMP_FILE" "$BINARY_PATH"; then
    error "Failed to install binary to ${BINARY_PATH}. Check disk space and permissions."
    _TMP_FILE=""
    exit 1
  fi
  _TMP_FILE="" # clear so cleanup trap doesn't remove the installed binary
  info "Binary installed to ${BINARY_PATH}"

  # Version is recorded by the caller (do_install/do_upgrade) after
  # verifying the service starts, not here, to avoid "already up to date"
  # blocking recovery from a partial install.
  _DOWNLOADED_VERSION="$version"
}

# =============================================================================
# PATH management
# =============================================================================

ensure_path() {
  case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) return 0 ;;
  esac

  warn "${INSTALL_DIR} is not in your PATH"

  local shell_rc=""
  case "${SHELL:-/bin/bash}" in
    */zsh) shell_rc="${HOME}/.zshrc" ;;
    */bash) shell_rc="${HOME}/.bashrc" ;;
    *) shell_rc="${HOME}/.profile" ;;
  esac

  if prompt_yn "Add ${INSTALL_DIR} to PATH in ${shell_rc}?"; then
    if grep -qF "# Added by termbot installer" "$shell_rc" 2>/dev/null; then
      info "PATH entry already exists in ${shell_rc}"
    else
      printf '\n# Added by termbot installer\nexport PATH="%s:${PATH}"\n' "$INSTALL_DIR" >>"$shell_rc"
    fi
    info "PATH updated in ${shell_rc}"
    hint_always "Run: source ${shell_rc}  (or open a new terminal)"
    export PATH="${INSTALL_DIR}:${PATH}"
  else
    warn "Add ${INSTALL_DIR} to your PATH manually."
  fi
}

# =============================================================================
# Service PATH — discover where tmux and claude live at install time
# =============================================================================

build_service_path() {
  local paths="${INSTALL_DIR}:/usr/local/bin:/usr/bin:/bin"

  # Homebrew (Apple Silicon + Intel)
  [ -d "/opt/homebrew/bin" ] && paths="/opt/homebrew/bin:${paths}"

  # tmux directory
  local tmux_dir
  tmux_dir="$(dirname "$(command -v tmux 2>/dev/null)" 2>/dev/null)" || true
  case ":${paths}:" in *":${tmux_dir}:"*) ;; *) [ -n "$tmux_dir" ] && paths="${tmux_dir}:${paths}" ;; esac

  # claude directory
  local claude_dir
  claude_dir="$(dirname "$(command -v claude 2>/dev/null)" 2>/dev/null)" || true
  case ":${paths}:" in *":${claude_dir}:"*) ;; *) [ -n "$claude_dir" ] && paths="${claude_dir}:${paths}" ;; esac

  # npm global bin (common claude location)
  local npm_prefix npm_bin=""
  npm_prefix="$(npm config get prefix 2>/dev/null)" || true
  if [ -n "$npm_prefix" ] && [ -d "${npm_prefix}/bin" ]; then
    npm_bin="${npm_prefix}/bin"
  fi
  case ":${paths}:" in *":${npm_bin}:"*) ;; *) [ -n "$npm_bin" ] && paths="${npm_bin}:${paths}" ;; esac

  printf "%s" "$paths"
}

# =============================================================================
# Service installation — Linux (systemd user units)
# =============================================================================

install_service_linux() {
  if ! has_systemd; then
    warn "systemd not available -- skipping service installation."
    hint_always "Start termbot manually: TERMBOT_CONFIG=${CONFIG_FILE} ${BINARY_PATH} &"
    hint_always "Or add to crontab:      @reboot TERMBOT_CONFIG=${CONFIG_FILE} ${BINARY_PATH}"
    return 0
  fi

  ensure_user_bus

  step "Installing systemd service"

  local svc_path
  svc_path="$(build_service_path)"

  mkdir -p "$SYSTEMD_DIR"
  mkdir -p "$LOG_DIR"
  chmod 700 "$LOG_DIR"

  cat >"$SYSTEMD_SERVICE" <<EOF
[Unit]
Description=termbot - terminal control from Telegram/Slack
After=network-online.target
Wants=network-online.target
StartLimitIntervalSec=300
StartLimitBurst=5

[Service]
Type=simple
ExecStart=${BINARY_PATH}
WorkingDirectory=${HOME}
Environment=TERMBOT_CONFIG=${CONFIG_FILE}
Environment=PATH=${svc_path}
Restart=on-failure
RestartSec=10

# Send OS notification on crash (not on normal stop via SIGTERM)
ExecStopPost=-/bin/sh -c 'if [ "\$SERVICE_RESULT" = "exit-code" ]; then ${UPDATE_CHECK_PATH} --health-notify 2>/dev/null; fi'

[Install]
WantedBy=default.target
EOF

  if ! systemctl --user daemon-reload; then
    error "systemctl daemon-reload failed. Check that D-Bus session bus is available."
    hint_always "Try: export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/\$(id -u)/bus"
    return 1
  fi
  if ! systemctl --user enable termbot.service 2>&1; then
    warn "Failed to enable termbot.service. You may need to enable it manually."
  fi
  info "systemd service installed and enabled"

  # Check linger for persistence across logout
  if command -v loginctl >/dev/null 2>&1; then
    if ! loginctl show-user "$(id -un)" -p Linger 2>/dev/null | grep -q "yes"; then
      warn "User lingering is not enabled. Service may not survive logout."
      hint_always "Enable with: sudo loginctl enable-linger $(id -un)"
    fi
  fi
}

install_update_timer_linux() {
  if ! has_systemd; then
    return 0
  fi

  step "Installing daily update check"

  local svc_path
  svc_path="$(build_service_path)"

  cat >"$SYSTEMD_UPDATE_SERVICE" <<EOF
[Unit]
Description=Check for termbot updates

[Service]
Type=oneshot
ExecStart="${UPDATE_CHECK_PATH}"
Environment=PATH=${svc_path}
EOF

  cat >"$SYSTEMD_UPDATE_TIMER" <<EOF
[Unit]
Description=Daily termbot update check

[Timer]
OnCalendar=daily
Persistent=true
RandomizedDelaySec=3600

[Install]
WantedBy=timers.target
EOF

  systemctl --user daemon-reload
  systemctl --user enable --now termbot-update-check.timer >/dev/null 2>&1
  info "Daily update check timer installed"
}

# =============================================================================
# Service installation — macOS (launchd plists)
# =============================================================================

install_service_macos() {
  step "Installing launchd service"

  local svc_path
  svc_path="$(build_service_path)"

  mkdir -p "$(dirname "$LAUNCHD_SERVICE")"
  mkdir -p "$LOG_DIR"
  chmod 700 "$LOG_DIR"

  cat >"$LAUNCHD_SERVICE" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.termbot.agent</string>
    <key>ProgramArguments</key>
    <array>
        <string>$(xml_escape "$WRAPPER_PATH")</string>
    </array>
    <key>WorkingDirectory</key>
    <string>$(xml_escape "$HOME")</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>TERMBOT_CONFIG</key>
        <string>$(xml_escape "$CONFIG_FILE")</string>
        <key>PATH</key>
        <string>$(xml_escape "$svc_path")</string>
    </dict>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>StandardOutPath</key>
    <string>$(xml_escape "${LOG_DIR}/termbot.log")</string>
    <key>StandardErrorPath</key>
    <string>$(xml_escape "${LOG_DIR}/termbot.log")</string>
    <key>ThrottleInterval</key>
    <integer>10</integer>
</dict>
</plist>
EOF

  info "launchd service installed"
}

install_update_timer_macos() {
  step "Installing daily update check"

  cat >"$LAUNCHD_UPDATE" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.termbot.update-check</string>
    <key>ProgramArguments</key>
    <array>
        <string>$(xml_escape "$UPDATE_CHECK_PATH")</string>
    </array>
    <key>StartCalendarInterval</key>
    <dict>
        <key>Hour</key>
        <integer>9</integer>
        <key>Minute</key>
        <integer>0</integer>
    </dict>
    <key>StandardOutPath</key>
    <string>$(xml_escape "${LOG_DIR}/update-check.log")</string>
    <key>StandardErrorPath</key>
    <string>$(xml_escape "${LOG_DIR}/update-check.log")</string>
</dict>
</plist>
EOF

  info "Daily update check installed (runs at 9:00 AM)"
}

# macOS crash-notification wrapper (launchd lacks ExecStopPost)
write_wrapper_script() {
  cat >"$WRAPPER_PATH" <<EOF
#!/usr/bin/env bash
# termbot wrapper — forward signals, notify on crash
if [ ! -x "${BINARY_PATH}" ]; then
  echo "termbot binary not found: ${BINARY_PATH}" >&2
  exit 1
fi

"${BINARY_PATH}" &
child=\$!
trap 'kill -TERM \$child 2>/dev/null; wait \$child' TERM INT
wait \$child
ec=\$?

# 143 = SIGTERM (normal stop), don't notify
if [ \$ec -ne 0 ] && [ \$ec -ne 143 ]; then
  "${UPDATE_CHECK_PATH}" --health-notify 2>/dev/null || true
fi
exit \$ec
EOF
  chmod +x "$WRAPPER_PATH"
}

# =============================================================================
# Update check script (installed to ~/.local/bin/termbot-update-check)
# =============================================================================

write_update_check_script() {
  cat >"$UPDATE_CHECK_PATH" <<'OUTER'
#!/usr/bin/env bash
set -euo pipefail

CONFIG_DIR="${HOME}/.config/termbot"
VERSION_FILE="${CONFIG_DIR}/.installed-version"
LOG_DIR="${CONFIG_DIR}/logs"
REPO="shegx01/termbot"
API="https://api.github.com/repos/${REPO}/releases/latest"

mkdir -p "$LOG_DIR"
chmod 700 "$LOG_DIR"

log() { printf "[%s] %s\n" "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" "$1" >> "${LOG_DIR}/update-check.log"; }

notify() {
  local title="$1" message="$2"
  case "$(uname -s)" in
    Darwin)
      osascript \
        -e 'on run argv' \
        -e 'display notification (item 2 of argv) with title (item 1 of argv)' \
        -e 'end run' \
        -- "$title" "$message" 2>/dev/null || true
      ;;
    Linux)
      command -v notify-send >/dev/null 2>&1 && notify-send "$title" "$message" 2>/dev/null || true
      ;;
  esac
  log "${title}: ${message}"
}

# --health-notify: called by service wrapper on crash
if [ "${1:-}" = "--health-notify" ]; then
  notify "termbot" "Service crashed -- restarting automatically..."
  exit 0
fi

# Health check: is the service running?
check_health() {
  case "$(uname -s)" in
    Darwin)
      if ! launchctl list com.termbot.agent >/dev/null 2>&1; then
        notify "termbot" "Service is not running. Check: launchctl list com.termbot.agent"
      fi
      ;;
    Linux)
      if command -v systemctl >/dev/null 2>&1; then
        if ! systemctl --user is-active --quiet termbot.service 2>/dev/null; then
          notify "termbot" "Service is not running. Check: systemctl --user status termbot"
        fi
      else
        if ! pgrep -x termbot >/dev/null 2>&1; then
          notify "termbot" "Service is not running."
        fi
      fi
      ;;
  esac
}

# Update check: is a newer release available?
check_update() {
  local installed latest
  installed="$(cat "$VERSION_FILE" 2>/dev/null || printf "unknown")"
  latest="$(curl -fsSL "$API" 2>/dev/null | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/' || printf "")"

  if [ -z "$latest" ]; then
    log "Update check failed (network error?)"
    return 0
  fi

  if [ "$installed" != "$latest" ] && [ "$latest" != "unknown" ]; then
    notify "termbot update available" "${latest} (installed: ${installed}). Run: install.sh --upgrade"
  else
    log "Up to date: ${installed}"
  fi
}

check_health
check_update
OUTER

  chmod +x "$UPDATE_CHECK_PATH"
  info "Update check script installed to ${UPDATE_CHECK_PATH}"
}

# =============================================================================
# Service control
# =============================================================================

start_service() {
  local os
  os="$(detect_os)"

  step "Starting termbot"

  case "$os" in
    macos)
      launchctl bootstrap "gui/$(id -u)" "$LAUNCHD_SERVICE" 2>/dev/null \
        || launchctl load "$LAUNCHD_SERVICE" 2>/dev/null \
        || true
      launchctl bootstrap "gui/$(id -u)" "$LAUNCHD_UPDATE" 2>/dev/null \
        || launchctl load "$LAUNCHD_UPDATE" 2>/dev/null \
        || true
      ;;
    linux)
      if has_systemd; then
        ensure_user_bus
        systemctl --user start termbot.service || true
      else
        # No systemd — start directly in background
        TERMBOT_CONFIG="$CONFIG_FILE" nohup "$BINARY_PATH" >>"${LOG_DIR}/termbot.log" 2>&1 &
      fi
      ;;
  esac

  sleep 2

  if verify_running; then
    info "termbot is running"
  else
    warn "termbot may not have started correctly."
    hint_always "Check logs: tail -f ${LOG_DIR}/termbot.log"
  fi
}

stop_service() {
  local os
  os="$(detect_os)"

  case "$os" in
    macos)
      launchctl bootout "gui/$(id -u)/com.termbot.agent" 2>/dev/null \
        || launchctl unload "$LAUNCHD_SERVICE" 2>/dev/null \
        || true
      launchctl bootout "gui/$(id -u)/com.termbot.update-check" 2>/dev/null \
        || launchctl unload "$LAUNCHD_UPDATE" 2>/dev/null \
        || true
      ;;
    linux)
      if has_systemd; then
        ensure_user_bus
        systemctl --user stop termbot.service 2>/dev/null || true
        systemctl --user stop termbot-update-check.timer 2>/dev/null || true
        systemctl --user disable termbot.service 2>/dev/null || true
        systemctl --user disable termbot-update-check.timer 2>/dev/null || true
      else
        # No systemd — kill by PID
        pkill -x termbot 2>/dev/null || true
      fi
      ;;
  esac
}

restart_service() {
  local os
  os="$(detect_os)"

  case "$os" in
    macos)
      launchctl bootout "gui/$(id -u)/com.termbot.agent" 2>/dev/null \
        || launchctl unload "$LAUNCHD_SERVICE" 2>/dev/null \
        || true
      # Wait for old process to actually exit (up to 10 seconds)
      local i=0
      while pgrep -x termbot >/dev/null 2>&1 && [ $i -lt 10 ]; do
        sleep 1
        i=$((i + 1))
      done
      launchctl bootstrap "gui/$(id -u)" "$LAUNCHD_SERVICE" 2>/dev/null \
        || launchctl load "$LAUNCHD_SERVICE" 2>/dev/null \
        || true
      ;;
    linux)
      if has_systemd; then
        ensure_user_bus
        systemctl --user restart termbot.service 2>/dev/null \
          || systemctl --user start termbot.service 2>/dev/null \
          || true
      else
        pkill -x termbot 2>/dev/null || true
        sleep 1
        TERMBOT_CONFIG="$CONFIG_FILE" nohup "$BINARY_PATH" >>"${LOG_DIR}/termbot.log" 2>&1 &
      fi
      ;;
  esac
}

verify_running() {
  local os
  os="$(detect_os)"

  case "$os" in
    macos) pgrep -x termbot >/dev/null 2>&1 ;;
    linux)
      if has_systemd; then
        systemctl --user is-active --quiet termbot.service 2>/dev/null
      else
        pgrep -x termbot >/dev/null 2>&1
      fi
      ;;
  esac
}

# =============================================================================
# Main flows
# =============================================================================

do_install() {
  local os target
  os="$(detect_os)"
  target="$(get_target)"

  # Banner
  printf "\n" >&2
  printf "  %btermbot installer%b\n" "$BOLD" "$NC" >&2
  printf "  %bhttps://github.com/%s%b\n" "$DIM" "$REPO" "$NC" >&2

  # Already installed?
  if [ -f "$BINARY_PATH" ]; then
    local installed_version
    installed_version="$(cat "$VERSION_FILE" 2>/dev/null || printf "unknown")"
    printf "\n" >&2
    info "termbot is already installed (${installed_version})"

    if prompt_yn "Upgrade to the latest version?"; then
      do_upgrade
      return
    elif prompt_yn "Reinstall from scratch? (config will be preserved)"; then
      stop_service # stop before replacing the binary
    else
      info "No changes made."
      exit 0
    fi
  fi

  # System info
  step "Checking system"
  info "OS: $(uname -s) (${os})"
  info "Architecture: $(uname -m)"
  info "Target: ${target}"

  # Dependencies
  step "Checking dependencies"
  check_curl
  check_tmux
  check_claude

  # Interactive config (skip if config already exists)
  if [ -f "$CONFIG_FILE" ]; then
    info "Existing config found at ${CONFIG_FILE}"
    if ! prompt_yn "Overwrite existing config?"; then
      info "Keeping existing config."
    else
      setup_config
    fi
  else
    setup_config
  fi

  # Download binary
  download_binary "$target"

  # PATH
  ensure_path

  # Helper scripts (must exist before service files reference them)
  write_update_check_script
  if [ "$os" = "macos" ]; then
    write_wrapper_script
  fi

  # Service + timer
  case "$os" in
    macos)
      install_service_macos
      install_update_timer_macos
      ;;
    linux)
      install_service_linux
      install_update_timer_linux
      ;;
  esac

  # Start
  start_service

  # Record version only after successful start
  mkdir -p "$CONFIG_DIR"
  if [ -n "$_DOWNLOADED_VERSION" ] && [ "$_DOWNLOADED_VERSION" != "unknown" ]; then
    printf "%s" "$_DOWNLOADED_VERSION" >"$VERSION_FILE"
  fi

  # Summary
  print_summary "$os"
}

setup_config() {
  require_tty

  step "Configuration"
  prompt_platform_choice

  if [ "$PLATFORM" = "telegram" ] || [ "$PLATFORM" = "both" ]; then
    prompt_telegram
  fi

  if [ "$PLATFORM" = "slack" ] || [ "$PLATFORM" = "both" ]; then
    prompt_slack
  fi

  step "Writing configuration"
  write_config
}

do_upgrade() {
  local os target
  os="$(detect_os)"
  target="$(get_target)"

  printf "\n" >&2
  printf "  %btermbot upgrade%b\n\n" "$BOLD" "$NC" >&2

  if [ ! -f "$BINARY_PATH" ]; then
    error "termbot is not installed. Run install.sh without flags to install."
    exit 1
  fi

  # Populate release cache in parent shell
  prefetch_release_info

  local installed_version latest_version
  installed_version="$(cat "$VERSION_FILE" 2>/dev/null || printf "unknown")"
  latest_version="$(get_latest_version)"

  info "Installed: ${installed_version}"
  info "Latest:    ${latest_version}"

  if [ "$installed_version" = "$latest_version" ] && [ "$installed_version" != "unknown" ]; then
    info "Already up to date!"
    exit 0
  fi

  if [ "$latest_version" = "unknown" ]; then
    warn "Could not determine latest version (GitHub API may be rate-limited)."
    if ! prompt_yn "Proceed with upgrade anyway?"; then
      exit 0
    fi
  fi

  # Download (handles atomic swap + backup)
  download_binary "$target"

  # Refresh helper scripts
  write_update_check_script
  if [ "$os" = "macos" ]; then
    write_wrapper_script
  fi

  # Refresh service files (updates PATH, fixes stale references)
  case "$os" in
    macos)
      install_service_macos
      install_update_timer_macos
      ;;
    linux)
      if has_systemd; then
        install_service_linux
        install_update_timer_linux
      fi
      ;;
  esac

  # Restart
  step "Restarting service"
  restart_service
  sleep 2

  if verify_running; then
    info "termbot upgraded and running (${latest_version})"
    hint_always "Previous version backed up to ${BACKUP_PATH}"
    # Record version only after verified start
    mkdir -p "$CONFIG_DIR"
    if [ -n "$_DOWNLOADED_VERSION" ] && [ "$_DOWNLOADED_VERSION" != "unknown" ]; then
      printf "%s" "$_DOWNLOADED_VERSION" >"$VERSION_FILE"
    fi
  else
    warn "New version failed to start. Auto-rolling back..."
    if [ -f "$BACKUP_PATH" ]; then
      mv "$BACKUP_PATH" "$BINARY_PATH"
      if [ -n "$installed_version" ] && [ "$installed_version" != "unknown" ]; then
        printf "%s" "$installed_version" >"$VERSION_FILE"
      fi
      restart_service
      sleep 2
      if verify_running; then
        info "Rolled back to ${installed_version} successfully."
      else
        error "Rollback also failed. Check logs: ${LOG_DIR}/termbot.log"
      fi
    else
      error "No backup available for rollback."
      hint_always "Check logs: tail -f ${LOG_DIR}/termbot.log"
    fi
  fi
}

do_uninstall() {
  local os
  os="$(detect_os)"

  printf "\n" >&2
  printf "  %btermbot uninstall%b\n\n" "$BOLD" "$NC" >&2

  # Stop services
  step "Stopping services"
  stop_service
  info "Services stopped"

  # Remove binary, backup, and helpers
  step "Removing files"
  for f in "$BINARY_PATH" "$BACKUP_PATH" "$UPDATE_CHECK_PATH" "$WRAPPER_PATH"; do
    if [ -f "$f" ]; then
      rm -f "$f"
      info "Removed ${f}"
    fi
  done

  # Remove service files
  case "$os" in
    macos)
      for f in "$LAUNCHD_SERVICE" "$LAUNCHD_UPDATE"; do
        if [ -f "$f" ]; then
          rm -f "$f"
          info "Removed ${f}"
        fi
      done
      ;;
    linux)
      for f in "$SYSTEMD_SERVICE" "$SYSTEMD_UPDATE_SERVICE" "$SYSTEMD_UPDATE_TIMER"; do
        if [ -f "$f" ]; then
          rm -f "$f"
          info "Removed ${f}"
        fi
      done
      if has_systemd; then
        ensure_user_bus
        systemctl --user daemon-reload 2>/dev/null || true
      fi
      ;;
  esac

  # Remove logs and version file
  if [ -d "$LOG_DIR" ]; then
    rm -rf "$LOG_DIR"
    info "Removed logs"
  fi
  rm -f "$VERSION_FILE" 2>/dev/null || true

  # Config — ask, default to keep
  if [ -f "$CONFIG_FILE" ]; then
    printf "\n" >&2
    if prompt_yn "Delete config file (${CONFIG_FILE})?" "n"; then
      rm -f "$CONFIG_FILE"
      rmdir "$CONFIG_DIR" 2>/dev/null || true
      info "Config removed"
    else
      info "Config preserved at ${CONFIG_FILE}"
    fi
  fi

  printf "\n" >&2
  info "termbot has been uninstalled."
}

# =============================================================================
# Post-install summary
# =============================================================================

print_summary() {
  local os="$1"

  printf "\n  %b%btermbot is installed and running!%b\n\n" "$BOLD" "$GREEN" "$NC" >&2

  printf "  %bFiles%b\n" "$BOLD" "$NC" >&2
  printf "    Binary   %s\n" "$BINARY_PATH" >&2
  printf "    Config   %s\n" "$CONFIG_FILE" >&2
  printf "    Logs     %s\n" "${LOG_DIR}/termbot.log" >&2
  printf "\n" >&2

  printf "  %bCommands%b\n" "$BOLD" "$NC" >&2
  case "$os" in
    macos)
      printf "    Status     launchctl list com.termbot.agent\n" >&2
      printf "    Logs       tail -f %s/termbot.log\n" "$LOG_DIR" >&2
      printf "    Restart    launchctl kickstart -k gui/\$(id -u)/com.termbot.agent\n" >&2
      ;;
    linux)
      if has_systemd; then
        printf "    Status     systemctl --user status termbot\n" >&2
        printf "    Logs       journalctl --user -u termbot -f\n" >&2
        printf "    Restart    systemctl --user restart termbot\n" >&2
      else
        printf "    Status     pgrep -x termbot\n" >&2
        printf "    Logs       tail -f %s/termbot.log\n" "$LOG_DIR" >&2
      fi
      ;;
  esac
  printf "    Upgrade    curl -sSL https://raw.githubusercontent.com/%s/main/install.sh | bash -s -- --upgrade\n" "$REPO" >&2
  printf "    Uninstall  curl -sSL https://raw.githubusercontent.com/%s/main/install.sh | bash -s -- --uninstall\n" "$REPO" >&2
  printf "\n" >&2

  printf "  %bOpen Telegram or Slack and send a message to your bot!%b\n\n" "$DIM" "$NC" >&2
}

# =============================================================================
# Argument parsing
# =============================================================================

show_help() {
  cat >&2 <<HELPEOF

  $(printf '%btermbot installer%b' "$BOLD" "$NC")

  $(printf '%bUsage%b' "$BOLD" "$NC")
    install.sh                 Install termbot interactively
    install.sh --quick         Install with minimal prompts
    install.sh --upgrade       Upgrade to the latest version
    install.sh --uninstall     Remove termbot

  $(printf '%bOptions%b' "$BOLD" "$NC")
    --quick, -q        Skip inline help text during setup
    --upgrade, -u      Download latest binary, restart service (config untouched)
    --uninstall        Stop service, remove files (config preserved by default)
    --help, -h         Show this help

  $(printf '%bExamples%b' "$BOLD" "$NC")
    # Fresh install via curl
    curl -sSL https://raw.githubusercontent.com/${REPO}/main/install.sh | bash

    # Install without help text
    curl -sSL https://raw.githubusercontent.com/${REPO}/main/install.sh | bash -s -- --quick

    # Upgrade
    curl -sSL https://raw.githubusercontent.com/${REPO}/main/install.sh | bash -s -- --upgrade

HELPEOF
}

parse_args() {
  while [ $# -gt 0 ]; do
    case "$1" in
      --quick | -q)
        QUICK=true
        shift
        ;;
      --upgrade | -u)
        UPGRADE=true
        shift
        ;;
      --uninstall | --remove)
        UNINSTALL=true
        shift
        ;;
      --help | -h)
        show_help
        exit 0
        ;;
      *)
        error "Unknown option: $1"
        show_help
        exit 1
        ;;
    esac
  done
}

# =============================================================================
# Entry point — wrapped in main() for curl|bash safety
# =============================================================================

main() {
  setup_colors

  # Validate environment
  if [ -z "${HOME:-}" ]; then
    error "\$HOME is not set. Cannot determine installation paths."
    exit 1
  fi

  # Register cleanup trap for interrupted downloads
  trap cleanup EXIT INT TERM

  parse_args "$@"

  if [ "$UNINSTALL" = true ]; then
    do_uninstall
    exit 0
  fi

  if [ "$UPGRADE" = true ]; then
    do_upgrade
    exit 0
  fi

  do_install
}

main "$@"
