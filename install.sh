#!/bin/sh
# reman installer for macOS and Linux.
#   curl -fsSL https://raw.githubusercontent.com/rdarshan10/reman/master/install.sh | sh
# Downloads the latest release for this machine, checks its SHA-256, runs `reman setup` (copies
# reman into ~/.reman/bin and starts the background daemon), and adds the shell integration to your
# zsh / bash / fish config. Options (environment variables):
#   REMAN_VERSION=v0.1.0   install that release instead of the latest
#   REMAN_NO_RC=1          don't touch your shell config
#   REMAN_NO_CONNECT=1     don't connect AI agents (otherwise: `reman connect all`, undo with
#                          `reman disconnect all`)
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
"$tmp/$name/reman" setup

bin="$HOME/.reman/bin"

# plug into every AI agent that's installed (Claude Code, Codex, Cursor, VS Code, ...). Each config
# file gets a .reman-bak backup; `reman disconnect all` undoes it.
if [ "${REMAN_NO_CONNECT:-0}" != 1 ]; then
  echo ""
  "$bin/reman" connect all
fi

# shell integration: one marked line per rc file, added once
add_line() {
  rc="$1"
  line="$2"
  [ -f "$rc" ] || touch "$rc"
  if ! grep -q "# reman shell integration" "$rc" 2>/dev/null; then
    printf '\n%s  # reman shell integration\n' "$line" >>"$rc"
    echo "Added reman to $rc"
  fi
}

if [ "${REMAN_NO_RC:-0}" != 1 ]; then
  case "$(basename "${SHELL:-sh}")" in
    zsh) add_line "$HOME/.zshrc" "eval \"\$(\"$bin/reman\" init zsh)\"" ;;
    bash) add_line "$HOME/.bashrc" "eval \"\$(\"$bin/reman\" init bash)\"" ;;
    fish)
      mkdir -p "$HOME/.config/fish"
      add_line "$HOME/.config/fish/config.fish" "\"$bin/reman\" init fish | source"
      ;;
    *) echo "Add reman to your shell: eval \"\$(\"$bin/reman\" init zsh)\"   (or bash / fish)" ;;
  esac
fi

echo ""
echo "reman is installed. Open a new terminal, then:"
echo "  press Up for the finder, Ctrl+R to search everywhere"
echo "  AI agents: reman connect   (see what's connected; reman disconnect all to undo)"
