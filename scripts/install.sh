#!/bin/sh
# Install the cgr binary from a GitHub release (Linux and macOS).
#
#   curl -fsSL https://raw.githubusercontent.com/NianJiuZst/CodeGraph-Rust/main/scripts/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/NianJiuZst/CodeGraph-Rust/main/scripts/install.sh | sh -s -- --skill
#
# Environment:
#   CGR_VERSION        version to install, e.g. 0.3.2 (default: the latest release)
#   CGR_INSTALL_DIR    where the binary goes (default: $HOME/.local/bin)
#   CGR_INSTALL_SKILL  set to 1 to also install the official SKILL.md (same as --skill)
#   CGR_BASE_URL       release download base URL (default: the GitHub releases of this repository);
#                      <base>/v<version>/cgr-<version>-<target>.tar.gz and its .sha256 must exist there
set -eu

REPO="NianJiuZst/CodeGraph-Rust"
BASE="${CGR_BASE_URL:-https://github.com/$REPO/releases/download}"
DEST="${CGR_INSTALL_DIR:-$HOME/.local/bin}"
INSTALL_SKILL="${CGR_INSTALL_SKILL:-0}"
SKILL_FLAGS=""
SKILL_DIR=""

while [ $# -gt 0 ]; do
  case "$1" in
    --skill|--with-skill) INSTALL_SKILL=1 ;;
    --claude|--claude-code|--codex|--antigravity|--gemini|--cursor|--opencode|--all)
      INSTALL_SKILL=1
      SKILL_FLAGS="$SKILL_FLAGS $1"
      ;;
    --skill-dir)
      shift
      [ $# -gt 0 ] || { printf 'install: --skill-dir requires a path\n' >&2; exit 1; }
      INSTALL_SKILL=1
      SKILL_DIR="$1"
      ;;
    *)
      printf 'install: unknown option: %s\n' "$1" >&2
      exit 1
      ;;
  esac
  shift
done

say() { printf '%s\n' "$*"; }
die() { printf 'install: %s\n' "$*" >&2; exit 1; }

case "$(uname -s)" in
  Linux) os=unknown-linux-musl;;
  Darwin) os=apple-darwin;;
  *) die "unsupported operating system $(uname -s); on Windows use install.ps1";;
esac
case "$(uname -m)" in
  x86_64|amd64) arch=x86_64;;
  aarch64|arm64) arch=aarch64;;
  *) die "unsupported CPU architecture $(uname -m)";;
esac
target="$arch-$os"

fetch() { # fetch <url> <file>
  if command -v curl >/dev/null 2>&1; then curl -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then wget -q "$1" -O "$2"
  else die "need curl or wget"; fi
}

version="${CGR_VERSION:-}"
if [ -z "$version" ]; then
  command -v curl >/dev/null 2>&1 || die "set CGR_VERSION (looking up the latest release needs curl)"
  final="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest")" || die "cannot look up the latest release"
  version="${final##*/v}"
  [ -n "$version" ] && [ "$version" != "$final" ] || die "no release has been published yet"
fi
version="${version#v}"

name="cgr-$version-$target"
tmp="$(mktemp -d 2>/dev/null || mktemp -d -t cgr-install)"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "downloading $name"
fetch "$BASE/v$version/$name.tar.gz" "$tmp/$name.tar.gz" || die "no release archive for $target at version $version"
fetch "$BASE/v$version/$name.tar.gz.sha256" "$tmp/$name.tar.gz.sha256" || die "no checksum file for $name"

expected="$(awk '{print $1}' "$tmp/$name.tar.gz.sha256")"
if command -v sha256sum >/dev/null 2>&1; then actual="$(sha256sum "$tmp/$name.tar.gz" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then actual="$(shasum -a 256 "$tmp/$name.tar.gz" | awk '{print $1}')"
else die "need sha256sum or shasum to verify the download"; fi
[ "$expected" = "$actual" ] || die "checksum mismatch (expected $expected, got $actual)"

tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
mkdir -p "$DEST"
install -m 755 "$tmp/$name/cgr" "$DEST/cgr" 2>/dev/null || { cp "$tmp/$name/cgr" "$DEST/cgr" && chmod 755 "$DEST/cgr"; }
say "installed $DEST/cgr ($("$DEST/cgr" version | head -1))"

case ":$PATH:" in
  *":$DEST:"*) ;;
  *) say "note: $DEST is not on your PATH; add it, e.g.  export PATH=\"$DEST:\$PATH\"";;
esac

if [ "$INSTALL_SKILL" = "1" ]; then
  skill_script="$tmp/install-skill.sh"
  skill_script_url="${CGR_SKILL_SCRIPT_URL:-https://raw.githubusercontent.com/$REPO/main/scripts/install-skill.sh}"
  if [ -n "${CGR_SKILL_SCRIPT_SOURCE:-}" ] && [ -f "$CGR_SKILL_SCRIPT_SOURCE" ]; then
    cp "$CGR_SKILL_SCRIPT_SOURCE" "$skill_script"
  else
    fetch "$skill_script_url" "$skill_script" || die "failed to download $skill_script_url"
  fi
  if [ -n "$SKILL_DIR" ]; then
    # shellcheck disable=SC2086
    PATH="$DEST:$PATH" sh "$skill_script" $SKILL_FLAGS --dir "$SKILL_DIR"
  else
    # shellcheck disable=SC2086
    PATH="$DEST:$PATH" sh "$skill_script" $SKILL_FLAGS
  fi
fi

say "next:  cd your-project && cgr init      # build the index"
if [ "$INSTALL_SKILL" != "1" ]; then
  say "       curl -fsSL https://raw.githubusercontent.com/$REPO/main/scripts/install-skill.sh | sh  # install SKILL.md"
fi
say "       cgr install                      # register MCP with Claude Code, Cursor and Codex"
