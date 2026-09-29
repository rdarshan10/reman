#!/bin/sh
# reman installer for macOS and Linux.
#   curl -fsSL https://raw.githubusercontent.com/rdarshan10/reman/master/install.sh | sh
# Downloads the latest release for this machine, checks its SHA-256, and runs `reman setup`, which
# does all of onboarding: copies reman into ~/.reman/bin, starts the background daemon, adds the
# shell integration to your zsh / bash / fish config, and connects every coding tool it finds
# (Claude Code, Codex, Cursor, ...). Change any of it later with `reman settings`.
# Options (environment variables):
#   REMAN_VERSION=v0.1.0   install that release instead of the latest
#   REMAN_NO_RC=1          don't touch your shell config
#   REMAN_NO_CONNECT=1     don't connect coding tools (undo later with `reman disconnect all`)
set -eu

REPO="rdarshan10/reman"
VERSION="${REMAN_VERSION:-latest}"

os=$(uname -s)
arch=$(uname -m)
case "$os-$arch" in
  Darwin-arm64) name=reman-macos-arm64 ;;
  Linux-x86_64) name=reman-linux-x64 ;;
  Darwin-x86_64)
    echo "reman: Intel Macs aren't supported (ONNX Runtime, which reman's search uses, publishes no build for them). Apple Silicon Macs are." >&2
    exit 1
    ;;
  *)
    echo "reman: no prebuilt binary for $os $arch yet. Build from source: https://github.com/$REPO#from-source" >&2
    exit 1
    ;;
esac

if [ "$VERSION" = latest ]; then
  base="https://github.com/$REPO/releases/latest/download"
else
  base="https://github.com/$REPO/releases/download/$VERSION"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Downloading $name ($VERSION)..."
curl -fsSL "$base/$name.tar.gz" -o "$tmp/$name.tar.gz"
curl -fsSL "$base/SHA256SUMS.txt" -o "$tmp/SHA256SUMS.txt"

want=$(grep " \*\{0,1\}$name.tar.gz\$" "$tmp/SHA256SUMS.txt" | cut -d' ' -f1)
if command -v sha256sum >/dev/null 2>&1; then
  got=$(sha256sum "$tmp/$name.tar.gz" | cut -d' ' -f1)
else
  got=$(shasum -a 256 "$tmp/$name.tar.gz" | cut -d' ' -f1)
fi
if [ -z "$want" ] || [ "$want" != "$got" ]; then
  echo "reman: checksum mismatch for $name.tar.gz (expected $want, got $got). Not installing." >&2
  exit 1
fi
echo "Checksum OK."

tar -xzf "$tmp/$name.tar.gz" -C "$tmp"

set -- setup
[ "${REMAN_NO_RC:-0}" = 1 ] && set -- "$@" --no-profile
[ "${REMAN_NO_CONNECT:-0}" = 1 ] && set -- "$@" --no-connect
"$tmp/$name/reman" "$@"

echo ""
echo "reman is installed. Open a new terminal, then:"
echo "  press Up for the finder, Ctrl+R to search everywhere"
echo "  reman settings   coding tools, shared folders, privacy"
