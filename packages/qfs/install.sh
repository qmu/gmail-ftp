#!/usr/bin/env sh
# qfs installer (ticket t40, RFD-0001 §1/§9).
#
# Detects your OS + arch, downloads the matching `qfs` release tarball, VERIFIES its sha256
# BEFORE extracting (RFD §9 reproducibility/observability; never run an unverified binary), and
# installs `qfs` to ~/.local/bin (override with QFS_INSTALL_DIR). No credential is ever fetched,
# stored, or required (RFD §10) — this only downloads a public release artifact + its checksum.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/qmu/qfs/main/packages/qfs/install.sh | sh
#   QFS_VERSION=v0.1.0 QFS_INSTALL_DIR=/usr/local/bin sh install.sh
set -eu

REPO="qmu/qfs"
INSTALL_DIR="${QFS_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${QFS_VERSION:-latest}"

say()  { printf 'qfs-install: %s\n' "$1" >&2; }
die()  { printf 'qfs-install: error: %s\n' "$1" >&2; exit 1; }

# --- Detect OS + arch and map to a release target triple ---------------------------------------
detect_target() {
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os" in
    Linux)  os_part="unknown-linux-musl" ;;
    Darwin) os_part="apple-darwin" ;;
    *) die "unsupported OS: $os (supported: Linux, macOS)" ;;
  esac
  case "$arch" in
    x86_64|amd64)  arch_part="x86_64" ;;
    aarch64|arm64) arch_part="aarch64" ;;
    *) die "unsupported arch: $arch (supported: x86_64, aarch64)" ;;
  esac
  printf '%s-%s' "$arch_part" "$os_part"
}

# --- Resolve the download base for the requested version ---------------------------------------
download_base() {
  if [ "$VERSION" = "latest" ]; then
    printf 'https://github.com/%s/releases/latest/download' "$REPO"
  else
    printf 'https://github.com/%s/releases/download/%s' "$REPO" "$VERSION"
  fi
}

# --- Pick a downloader (curl or wget) ----------------------------------------------------------
fetch() {  # fetch <url> <dest>
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$2" "$1"
  else
    die "need curl or wget to download"
  fi
}

# --- sha256 verification (portable: sha256sum or shasum -a 256) ---------------------------------
sha256_of() {  # sha256_of <file> -> hex on stdout
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    die "need sha256sum or shasum to verify the download"
  fi
}

main() {
  target="$(detect_target)"
  base="$(download_base)"
  tarball="qfs-${target}.tar.gz"
  url="${base}/${tarball}"
  sha_url="${url}.sha256"

  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT

  say "downloading ${tarball} (${VERSION})"
  fetch "$url" "$tmp/$tarball"
  fetch "$sha_url" "$tmp/$tarball.sha256"

  # Verify the sha256 BEFORE extracting. The .sha256 file is `<hex>  <name>` (sha256sum format).
  want="$(awk '{print $1}' "$tmp/$tarball.sha256")"
  got="$(sha256_of "$tmp/$tarball")"
  [ -n "$want" ] || die "empty checksum in $tarball.sha256"
  if [ "$want" != "$got" ]; then
    die "checksum mismatch for $tarball (expected $want, got $got) — refusing to install"
  fi
  say "sha256 verified: $got"

  # Extract and install only after verification.
  tar -xzf "$tmp/$tarball" -C "$tmp"
  [ -f "$tmp/qfs" ] || die "tarball did not contain a 'qfs' binary"
  mkdir -p "$INSTALL_DIR"
  install -m 0755 "$tmp/qfs" "$INSTALL_DIR/qfs" 2>/dev/null || {
    cp "$tmp/qfs" "$INSTALL_DIR/qfs"; chmod 0755 "$INSTALL_DIR/qfs";
  }

  say "installed qfs to $INSTALL_DIR/qfs"
  case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) say "note: $INSTALL_DIR is not on your PATH — add it to run 'qfs' directly" ;;
  esac
  "$INSTALL_DIR/qfs" --version || die "installed binary did not run"
}

main "$@"
