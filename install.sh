#!/bin/sh
# Install navigera: a prebuilt binary from the latest GitHub release, or a
# source build with cargo when no binary fits this platform.
#
#   curl -fsSL https://raw.githubusercontent.com/andrey-usa/navigera/master/install.sh | sh
#
# Env: NAVIGERA_INSTALL_DIR (default ~/.local/bin), NAVIGERA_VERSION
# (a tag such as v0.2.0; default: latest release), NAVIGERA_GIT_REV
# (commit for the source build; default: master).
#
# The archive is checked against the SHA-256 the release publishes next to
# it; a mismatch or a missing checksum stops the install
# (NAVIGERA_NO_VERIFY=1 skips the check).
set -eu

REPO="andrey-usa/navigera"
DIR="${NAVIGERA_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${NAVIGERA_VERSION:-latest}"

os=$(uname -s)
arch=$(uname -m)
case "$os/$arch" in
  Linux/x86_64) target=x86_64-unknown-linux-musl ;;
  Linux/aarch64 | Linux/arm64) target=aarch64-unknown-linux-musl ;;
  Darwin/arm64) target=aarch64-apple-darwin ;;
  Darwin/x86_64) target=x86_64-apple-darwin ;;
  *) target="" ;;
esac

if [ "$VERSION" = latest ]; then
  url="https://github.com/$REPO/releases/latest/download/navigera-$target.tar.gz"
else
  url="https://github.com/$REPO/releases/download/$VERSION/navigera-$target.tar.gz"
fi

mkdir -p "$DIR"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

fetch() { curl --proto '=https' --tlsv1.2 -fsSL --retry 3 -o "$2" "$1"; }

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | cut -d' ' -f1
  else return 1
  fi
}

verify() {
  [ "${NAVIGERA_NO_VERIFY:-}" = 1 ] && return 0
  if ! fetch "$url.sha256" "$tmp/navigera.tar.gz.sha256" 2>/dev/null; then
    echo "error: no checksum published at $url.sha256 (NAVIGERA_NO_VERIFY=1 to install anyway)" >&2
    exit 1
  fi
  want=$(cut -d' ' -f1 < "$tmp/navigera.tar.gz.sha256")
  if ! got=$(sha256 "$tmp/navigera.tar.gz"); then
    echo "error: need sha256sum or shasum to verify the download (NAVIGERA_NO_VERIFY=1 to skip)" >&2
    exit 1
  fi
  if [ "$want" != "$got" ]; then
    echo "error: checksum mismatch for $url (want $want, got $got)" >&2
    exit 1
  fi
}

if [ -n "$target" ] && fetch "$url" "$tmp/navigera.tar.gz" 2>/dev/null; then
  verify
  tar -xzf "$tmp/navigera.tar.gz" -C "$tmp"
  install -m 0755 "$tmp/navigera" "$DIR/navigera"
  echo "installed $DIR/navigera ($target, $VERSION)"
elif command -v cargo >/dev/null 2>&1; then
  echo "no prebuilt binary for ${target:-$os/$arch}; building from source with cargo (about a minute)…"
  # The repo also holds Rust benchmark contenders, so name the package.
  cargo install --locked --git "https://github.com/$REPO" ${NAVIGERA_GIT_REV:+--rev "$NAVIGERA_GIT_REV"} navigera --root "$tmp/root"
  install -m 0755 "$tmp/root/bin/navigera" "$DIR/navigera"
  echo "installed $DIR/navigera (built from source)"
else
  echo "error: no prebuilt binary for ${target:-$os/$arch} and no cargo; install Rust (https://rustup.rs) and rerun" >&2
  exit 1
fi

case ":$PATH:" in
  *":$DIR:"*) ;;
  *) echo "note: $DIR is not on PATH; run: export PATH=\"$DIR:\$PATH\"" ;;
esac
"$DIR/navigera" --version
echo "next: navigera install-skill   # usage guide for your agent (./.agents/skills; --claude for ./.claude/skills)"
