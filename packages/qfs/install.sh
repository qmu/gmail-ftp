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
#   QFS_VERSION=v0.0.1 QFS_INSTALL_DIR=/usr/local/bin sh install.sh
set -eu

REPO="qmu/qfs"
INSTALL_DIR="${QFS_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${QFS_VERSION:-latest}"

say()  { printf 'qfs-install: %s\n' "$1" >&2; }
die()  { printf 'qfs-install: error: %s\n' "$1" >&2; exit 1; }

# Printed after a successful install: how to test it, authenticate, update, and find the docs.
# Every command here is real; the "try it" ones work offline with no credentials.
next_steps() {
  cat >&2 <<'EOF'

  ✓ qfs is installed. Next steps:

  1) Try it — runs entirely on your machine, no account, no network:
       qfs run "/local/etc |> select name, size, is_dir |> limit 5"   # list a folder
       echo '{"k":1,"name":"alpha"}' > /tmp/d.json
       qfs run "/local/tmp/d.json |> decode json |> encode yaml"       # convert a file
     Both return real output immediately.

  2) Preview a write — still no account; PREVIEW changes nothing:
       qfs run "INSERT INTO /mail/drafts VALUES ('alice@example.com','Hi','Body')"
     (qfs run PREVIEWs by default; add --commit to actually apply — applying a
     real change needs a connected account, next.)

  3) Connect a service — only needed to apply real changes:
     First export QFS_PASSPHRASE — a password you choose that encrypts the
     service logins you save on this machine. It is not any service's own
     password; it just locks the local file your saved logins live in. Keep it
     set for the shell that runs `connection add/list/remove`:
       read -rs QFS_PASSPHRASE; export QFS_PASSPHRASE   # no shell-history leak
     Then add the connection, piping the credential VALUE via stdin (never argv,
     which leaks into the process table + shell history):
       printf %s "$TOKEN" | qfs connection add mail work   # then: qfs connection list
     Your credential is stored locally and never printed back.

  4) Update qfs later — re-run the installer (always fetches the latest):
       curl -fsSL https://raw.githubusercontent.com/qmu/qfs/main/packages/qfs/install.sh | sh
     Pin a version with QFS_VERSION=vX.Y.Z.

  5) Learn more:
       https://github.com/qmu/qfs#readme
       https://github.com/qmu/qfs/blob/main/docs/guide/getting-started.md
       qfs skill      # the operating procedure for AI agents

EOF
}

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
  next_steps
}

main "$@"
