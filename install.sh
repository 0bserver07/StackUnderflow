#!/bin/sh
# staxtrace installer — prebuilt binaries, checksum-verified, no sudo.
#   curl -fsSL https://raw.githubusercontent.com/0bserver07/staxtrace/main/install.sh | sh
# Options (env):
#   STAX_VERSION      tag to install (default: latest release, e.g. v1.0.0)
#   STAX_INSTALL_DIR  target dir (default: ~/.local/bin)
set -eu

REPO="0bserver07/staxtrace"
INSTALL_DIR="${STAX_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

# --- resolve version ---------------------------------------------------------
if [ -n "${STAX_VERSION:-}" ]; then
  TAG="$STAX_VERSION"
else
  # Follow the /releases/latest redirect instead of hitting the API (no rate limit, no jq).
  TAG=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest" \
    | sed 's#.*/tag/##')
  [ -n "$TAG" ] || die "could not resolve the latest release tag"
fi

# --- resolve platform triple -------------------------------------------------
OS=$(uname -s)
ARCH=$(uname -m)
case "$OS-$ARCH" in
  Darwin-arm64)           TRIPLE="aarch64-apple-darwin" ;;
  Darwin-x86_64)          die "no Intel-macOS artifact ships yet for $TAG — use: cargo build --release (or an Apple Silicon machine)" ;;
  Linux-x86_64)           TRIPLE="x86_64-unknown-linux-gnu" ;;
  Linux-aarch64|Linux-arm64) TRIPLE="aarch64-unknown-linux-gnu" ;;
  *)                      die "unsupported platform: $OS $ARCH" ;;
esac

# The current Linux artifacts are glibc-linked and need glibc >= 2.39.
# This check disappears once static musl artifacts ship (see NEXT: release CI).
if [ "$OS" = "Linux" ]; then
  if command -v getconf >/dev/null 2>&1; then
    GLIBC=$(getconf GNU_LIBC_VERSION 2>/dev/null | awk '{print $2}' || true)
    case "$GLIBC" in
      2.*)
        MINOR=${GLIBC#2.}; MINOR=${MINOR%%.*}
        [ "$MINOR" -ge 39 ] 2>/dev/null || die "the $TAG Linux binaries need glibc >= 2.39; this system has $GLIBC (Ubuntu 24.04+ works). Until static musl builds ship: cargo build --release"
        ;;
      "") die "could not detect glibc (musl system?) — the $TAG binaries are glibc-linked; build from source: cargo build --release" ;;
    esac
  fi
fi

# --- download + verify -------------------------------------------------------
NAME="staxtrace-$TAG-$TRIPLE"
BASE="https://github.com/$REPO/releases/download/$TAG"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

say "staxtrace $TAG ($TRIPLE) -> $INSTALL_DIR"
curl -fsSL -o "$TMP/$NAME.tar.gz"        "$BASE/$NAME.tar.gz"        || die "download failed: $BASE/$NAME.tar.gz"
curl -fsSL -o "$TMP/$NAME.tar.gz.sha256" "$BASE/$NAME.tar.gz.sha256" || die "checksum download failed"

cd "$TMP"
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum -c "$NAME.tar.gz.sha256" >/dev/null || die "checksum mismatch — refusing to install"
elif command -v shasum >/dev/null 2>&1; then
  shasum -a 256 -c "$NAME.tar.gz.sha256" >/dev/null || die "checksum mismatch — refusing to install"
else
  die "need sha256sum or shasum to verify the download"
fi

tar -xzf "$NAME.tar.gz"

# --- install -----------------------------------------------------------------
mkdir -p "$INSTALL_DIR"
for BIN in stax stax-server stax-hooks; do
  [ -f "$NAME/$BIN" ] || die "release tarball is missing $BIN"
  install -m 0755 "$NAME/$BIN" "$INSTALL_DIR/$BIN"
done

say "installed: stax, stax-server, stax-hooks"
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) say "note: $INSTALL_DIR is not on your PATH — add:  export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac
say "next:  stax init"
