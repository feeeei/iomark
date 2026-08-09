#!/bin/sh
# iomark installer — https://iomark.dev
#
#   curl -fsSL https://iomark.dev | sh                     # install
#   curl -fsSL https://iomark.dev | sh -s -- quick         # run once, install nothing
#   curl -fsSL https://iomark.dev | sh -s -- quick /mnt    # ... on a specific disk
#   curl -fsSL https://iomark.dev | sh -s -- uninstall
#
# Works on macOS, Linux, and Windows under Git Bash / MSYS2. Native Windows
# PowerShell users want install.ps1 instead.
set -eu

REPO="${IOMARK_REPO:-feeeei/iomark}"
VERSION="${IOMARK_VERSION:-latest}"
INSTALL_DIR="${IOMARK_INSTALL_DIR:-}"
ASSET="${IOMARK_ASSET:-}"

CMD=install
TMPDIR_SELF=

say() { printf '%s\n' "$*" >&2; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}
need() { command -v "$1" >/dev/null 2>&1; }

# Re-exit with the status we were called with: a bare `rm` at the end of an
# EXIT trap would otherwise decide the script's exit code.
cleanup() {
  code=$?
  if [ -n "$TMPDIR_SELF" ]; then rm -rf "$TMPDIR_SELF" 2>/dev/null || :; fi
  exit "$code"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

usage() {
  cat <<'EOF'
iomark installer

Usage:
  install.sh [command] [options] [-- <iomark args>]

Commands:
  install      Download and install the iomark binary (default)
  quick        Download to a temp dir, run the benchmark, delete it
  uninstall    Remove a previously installed iomark
  help         Show this help

Options:
  --version <tag>   Release to fetch, e.g. v0.1.0 (default: latest)
  --dir <path>      Install directory (default: ~/.local/bin)
  --asset <name>    Override platform detection with a release asset name

Environment:
  IOMARK_VERSION, IOMARK_INSTALL_DIR, IOMARK_ASSET, IOMARK_REPO,
  IOMARK_BASE_URL (mirror serving the release assets and SHA256SUMS)

Anything after the command that is not an option above is passed to iomark
by `quick`, so `quick /mnt/data --size 4GiB` benchmarks /mnt/data.
EOF
}

# ---------------------------------------------------------------- arguments

if [ $# -gt 0 ]; then
  case $1 in
  install | quick | uninstall)
    CMD=$1
    shift
    ;;
  help | --help | -h)
    usage
    exit 0
    ;;
  esac
fi

while [ $# -gt 0 ]; do
  case $1 in
  --version | -v)
    VERSION=${2:?--version needs a tag}
    shift 2
    ;;
  --version=*)
    VERSION=${1#*=}
    shift
    ;;
  --dir | -d)
    INSTALL_DIR=${2:?--dir needs a path}
    shift 2
    ;;
  --dir=*)
    INSTALL_DIR=${1#*=}
    shift
    ;;
  --asset)
    ASSET=${2:?--asset needs a name}
    shift 2
    ;;
  --asset=*)
    ASSET=${1#*=}
    shift
    ;;
  --help | -h)
    usage
    exit 0
    ;;
  --)
    shift
    break
    ;;
  *) break ;;
  esac
done
# Whatever survives is for iomark itself.

# ----------------------------------------------------------------- platform

detect_asset() {
  os=$(uname -s 2>/dev/null || echo unknown)
  arch=$(uname -m 2>/dev/null || echo unknown)
  case $os in
  Linux)
    case $arch in
    x86_64 | amd64) echo iomark-x86_64-unknown-linux-gnu.tar.gz ;;
    aarch64 | arm64) echo iomark-aarch64-unknown-linux-gnu.tar.gz ;;
    *) die "unsupported Linux architecture: $arch" ;;
    esac
    ;;
  Darwin) echo iomark-macos-universal.tar.gz ;;
  MINGW* | MSYS* | CYGWIN* | Windows_NT)
    case $arch in
    x86_64 | amd64) echo iomark-x86_64-pc-windows-gnu.zip ;;
    aarch64 | arm64) echo iomark-aarch64-pc-windows-gnullvm.zip ;;
    *) die "unsupported Windows architecture: $arch" ;;
    esac
    ;;
  *) die "unsupported platform: $os $arch (see https://github.com/$REPO/releases)" ;;
  esac
}

is_windows() {
  case $(uname -s 2>/dev/null || echo unknown) in
  MINGW* | MSYS* | CYGWIN* | Windows_NT) return 0 ;;
  *) return 1 ;;
  esac
}

[ -n "$ASSET" ] || ASSET=$(detect_asset)
if is_windows; then BIN=iomark.exe; else BIN=iomark; fi

# IOMARK_BASE_URL points the downloads at a mirror; it must expose the release
# assets and SHA256SUMS side by side.
if [ -n "${IOMARK_BASE_URL:-}" ]; then
  BASE_URL=${IOMARK_BASE_URL%/}
elif [ "$REPO" != feeeei/iomark ]; then
  if [ "$VERSION" = latest ]; then
    BASE_URL="https://github.com/$REPO/releases/latest/download"
  else
    BASE_URL="https://github.com/$REPO/releases/download/$VERSION"
  fi
elif [ "$VERSION" = latest ]; then
  BASE_URL="https://iomark.dev/releases/latest/download"
else
  BASE_URL="https://iomark.dev/releases/download/$VERSION"
fi

# ----------------------------------------------------------------- download

fetch() { # fetch <url> <dest>
  if need curl; then
    curl -fsSL --retry 3 -o "$2" "$1"
  elif need wget; then
    wget -qO "$2" "$1"
  else
    die "neither curl nor wget is available"
  fi
}

sha256_of() {
  if need sha256sum; then
    sha256sum "$1" | cut -d' ' -f1
  elif need shasum; then
    shasum -a 256 "$1" | cut -d' ' -f1
  elif need openssl; then
    openssl dgst -sha256 "$1" | awk '{print $NF}'
  fi
}

verify() { # verify <file> <name>
  actual=$(sha256_of "$1" || true)
  if [ -z "${actual:-}" ]; then
    warn "no sha256 tool found, skipping checksum verification"
    return 0
  fi
  sums=$TMPDIR_SELF/SHA256SUMS
  if ! fetch "$BASE_URL/SHA256SUMS" "$sums" 2>/dev/null; then
    warn "could not download SHA256SUMS, skipping checksum verification"
    return 0
  fi
  expected=$(awk -v n="$2" '$2 == n || $2 == "*" n {print $1}' "$sums")
  [ -n "$expected" ] || die "$2 is missing from SHA256SUMS"
  [ "$expected" = "$actual" ] || die "checksum mismatch for $2 (expected $expected, got $actual)"
}

unpack() { # unpack <archive> <dir>
  case $1 in
  *.tar.gz)
    tar -xzf "$1" -C "$2"
    ;;
  *.zip)
    if need unzip; then
      unzip -q -o "$1" -d "$2"
    elif need 7z; then
      7z x -y -o"$2" "$1" >/dev/null
    elif need powershell; then
      powershell -NoProfile -Command \
        "Expand-Archive -Force -LiteralPath '$(cygpath -w "$1" 2>/dev/null || echo "$1")' -DestinationPath '$(cygpath -w "$2" 2>/dev/null || echo "$2")'"
    else
      die "need unzip, 7z, or powershell to extract $1"
    fi
    ;;
  *) die "unknown archive type: $1" ;;
  esac
}

download() { # -> $TMPDIR_SELF/$BIN
  TMPDIR_SELF=$(mktemp -d 2>/dev/null || mktemp -d -t iomark)
  archive=$TMPDIR_SELF/$ASSET
  say "downloading $ASSET ($VERSION)"
  fetch "$BASE_URL/$ASSET" "$archive" ||
    die "download failed: $BASE_URL/$ASSET"
  verify "$archive" "$ASSET"
  unpack "$archive" "$TMPDIR_SELF"
  [ -f "$TMPDIR_SELF/$BIN" ] || die "$BIN not found in $ASSET"
  chmod +x "$TMPDIR_SELF/$BIN"
}

# ----------------------------------------------------------------- commands

default_dir() {
  if is_windows; then
    echo "$HOME/bin"
  else
    echo "$HOME/.local/bin"
  fi
}

do_install() {
  [ -n "$INSTALL_DIR" ] || INSTALL_DIR=$(default_dir)
  download
  mkdir -p "$INSTALL_DIR" || die "cannot create $INSTALL_DIR"
  target=$INSTALL_DIR/$BIN
  # Replace by rename so a running iomark is not corrupted mid-write.
  mv -f "$TMPDIR_SELF/$BIN" "$target" 2>/dev/null ||
    cp -f "$TMPDIR_SELF/$BIN" "$target" ||
    die "cannot write $target (try --dir, or re-run with sudo)"
  say "installed $("$target" --version 2>/dev/null | head -n1) -> $target"

  case ":$PATH:" in
  *":$INSTALL_DIR:"*) say "run: iomark" ;;
  *)
    say ""
    say "$INSTALL_DIR is not in PATH. Add it to your shell profile:"
    say "  export PATH=\"$INSTALL_DIR:\$PATH\""
    say "or run it directly: $target"
    ;;
  esac
}

do_quick() {
  download
  say ""
  # curl | sh leaves stdin on the pipe, which would drop iomark into its
  # non-interactive plain renderer — hand it the terminal back when there is
  # one so the TUI runs.
  #
  # Duplicate stdout rather than opening /dev/tty: the pipeline only took
  # stdin away, so fd 1 is still the terminal device this shell inherited. A
  # fresh /dev/tty descriptor is a terminal too, but on macOS it cannot be
  # registered with kqueue, and iomark's key-event reader would not start.
  rc=0
  if [ ! -t 0 ] && [ -t 1 ] && (exec <&1) 2>/dev/null; then
    "$TMPDIR_SELF/$BIN" "$@" <&1 || rc=$?
  elif [ ! -t 0 ] && (exec </dev/tty) 2>/dev/null; then
    "$TMPDIR_SELF/$BIN" "$@" </dev/tty || rc=$?
  else
    "$TMPDIR_SELF/$BIN" "$@" || rc=$?
  fi
  say ""
  say "that binary was temporary — install it with:"
  say "  curl -fsSL https://iomark.dev | sh"
  return "$rc"
}

UNINSTALLED=0
remove_from() {
  [ -n "$1" ] && [ -f "$1/$BIN" ] || return 0
  UNINSTALLED=1
  if rm -f "$1/$BIN"; then say "removed $1/$BIN"; else warn "cannot remove $1/$BIN"; fi
}

do_uninstall() {
  remove_from "$INSTALL_DIR"
  remove_from "$(default_dir)"
  remove_from /usr/local/bin
  remove_from /opt/homebrew/bin
  [ "$UNINSTALLED" = 1 ] || say "no iomark installation found"
}

case $CMD in
install) do_install ;;
quick) do_quick "$@" ;;
uninstall) do_uninstall ;;
esac
