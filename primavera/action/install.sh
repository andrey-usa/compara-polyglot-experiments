#!/usr/bin/env bash
# The action's install step: a release binary for this runner (checked
# against the SHA-256 published beside it), else a build from the action's
# own commit with navigera beside it. Puts `primavera` on the PATH.
set -euo pipefail

# Git Bash on Windows: POSIX paths here, a Windows one for GITHUB_PATH.
unix() { cygpath -u "$1" 2>/dev/null || printf '%s\n' "$1"; }
PV_ACTION=$(unix "$PV_ACTION")
temp=$(unix "$RUNNER_TEMP")
root="$temp/primavera"
mkdir -p "$root/bin"
cygpath -w "$root/bin" >> "$GITHUB_PATH" 2>/dev/null || echo "$root/bin" >> "$GITHUB_PATH"

case "$RUNNER_OS/$RUNNER_ARCH" in
  Linux/X64) target=x86_64-unknown-linux-musl ext=tar.gz ;;
  Linux/ARM64) target=aarch64-unknown-linux-musl ext=tar.gz ;;
  macOS/ARM64) target=aarch64-apple-darwin ext=tar.gz ;;
  macOS/X64) target=x86_64-apple-darwin ext=tar.gz ;;
  Windows/X64) target=x86_64-pc-windows-msvc ext=zip ;;
  *) target="" ext="" ;;
esac

sha256() { if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1; }

release() {
  [ -n "$target" ] || return 1
  case "$PV_VERSION" in
    latest) url="https://github.com/andrey-usa/primavera/releases/latest/download/primavera-$target.$ext" ;;
    *) url="https://github.com/andrey-usa/primavera/releases/download/$PV_VERSION/primavera-$target.$ext" ;;
  esac
  tmp=$(mktemp -d)
  curl --proto '=https' --tlsv1.2 -fsSL --retry 3 -o "$tmp/pv.$ext" "$url" || return 1
  curl --proto '=https' --tlsv1.2 -fsSL --retry 3 -o "$tmp/pv.sha256" "$url.sha256" || {
    echo "::error title=primavera::no checksum beside $url"; exit 1; }
  [ "$(cut -d' ' -f1 < "$tmp/pv.sha256")" = "$(sha256 "$tmp/pv.$ext")" ] || {
    echo "::error title=primavera::$url does not match its checksum"; exit 1; }
  if [ "$ext" = zip ]; then unzip -qo "$tmp/pv.zip" -d "$root/bin"; else tar -C "$root/bin" -xzf "$tmp/pv.tar.gz"; fi
  echo "primavera $PV_VERSION from $url"
}

source_build() {
  # Cargo.toml takes navigera from ../navigera.
  nav="$PV_ACTION/../navigera"
  if [ ! -f "$nav/Cargo.toml" ]; then
    git clone --quiet --filter=blob:none https://github.com/andrey-usa/navigera "$nav"
    git -C "$nav" checkout --quiet "$PV_NAVIGERA_REF"
  fi
  echo "building primavera from $PV_ACTION with navigera $(git -C "$nav" rev-parse --short HEAD)"
  CARGO_TARGET_DIR="$temp/primavera-target" cargo install --quiet --locked --path "$PV_ACTION" --root "$root"
}

if [ "$PV_VERSION" = source ] || ! release; then
  [ "$PV_VERSION" = source ] || echo "::notice title=primavera::no release binary for ${target:-this runner} ($PV_VERSION); building from source"
  source_build
fi
"$root/bin/primavera" --version
