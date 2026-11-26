#!/bin/sh
# Install the official codegraph-rust Agent Skill (SKILL.md) for coding agents.
#
#   curl -fsSL https://raw.githubusercontent.com/NianJiuZst/CodeGraph-Rust/main/scripts/install-skill.sh | sh
#
# Targets (default: auto-detect installed agents; falls back to ~/.agents/skills):
#   --claude       Install for Claude Code   (${CLAUDE_CONFIG_DIR:-~/.claude}/skills)
#   --codex        Install for Codex         (${CODEX_HOME:-~/.codex}/skills)
#   --antigravity  Install for Antigravity   (${GEMINI_CONFIG_DIR:-~/.gemini/config}/skills)
#   --cursor       Install for Cursor        (~/.cursor/skills)
#   --opencode     Install for OpenCode      (${XDG_CONFIG_HOME:-~/.config}/opencode/skills)
#   --all          Install for all 5 agents above
#   --dir <path>   Install under <path>/codegraph-rust/SKILL.md
#
# Environment:
#   CGR_SKILL_REF     Git branch or tag to fetch from (default: main)
#   CGR_SKILL_URL     Override download URL for SKILL.md
#   CGR_SKILL_SOURCE  Local SKILL.md path (skips network download)
set -eu

REPO="NianJiuZst/CodeGraph-Rust"
REF="${CGR_SKILL_REF:-main}"
URL="${CGR_SKILL_URL:-https://raw.githubusercontent.com/$REPO/$REF/skills/codegraph-rust/SKILL.md}"

say() { printf '%s\n' "$*"; }
die() { printf 'install-skill: %s\n' "$*" >&2; exit 1; }

usage() {
  cat <<'EOF'
Usage: install-skill.sh [--claude] [--codex] [--antigravity] [--cursor] [--opencode] [--all] [--dir <path>]

Installs the official codegraph-rust SKILL.md for CLI-first coding agent workflows.
With no target flags, auto-detects installed agent config directories (falling back to ~/.agents/skills).
EOF
}

fetch() {
  if command -v curl >/dev/null 2>&1; then curl -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then wget -q "$1" -O "$2"
  else die "need curl or wget"; fi
}

CLAUDE_ROOT="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
CODEX_ROOT="${CODEX_HOME:-$HOME/.codex}"
ANTIGRAVITY_ROOT="${GEMINI_CONFIG_DIR:-$HOME/.gemini/config}"
CURSOR_ROOT="$HOME/.cursor"
OPENCODE_ROOT="${XDG_CONFIG_HOME:-$HOME/.config}/opencode"

targets=""
custom_dir=""
add_target() {
  case " $targets " in
    *" $1 "*) ;;
    *) targets="$targets $1" ;;
  esac
}

while [ $# -gt 0 ]; do
  case "$1" in
    --claude|--claude-code) add_target claude ;;
    --codex) add_target codex ;;
    --antigravity|--gemini) add_target antigravity ;;
    --cursor) add_target cursor ;;
    --opencode) add_target opencode ;;
    --all)
      add_target claude
      add_target codex
      add_target antigravity
      add_target cursor
      add_target opencode
      ;;
    --dir)
      shift
      [ $# -gt 0 ] || die "--dir requires a path argument"
      custom_dir="$1"
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown option: $1 (try --help)"
      ;;
  esac
  shift
done

tmp="$(mktemp -d 2>/dev/null || mktemp -d -t cgr-skill)"
trap 'rm -rf "$tmp"' EXIT INT TERM
src="$tmp/SKILL.md"

if [ -n "${CGR_SKILL_SOURCE:-}" ]; then
  [ -f "$CGR_SKILL_SOURCE" ] || die "local skill source not found: $CGR_SKILL_SOURCE"
  cp "$CGR_SKILL_SOURCE" "$src"
else
  say "downloading codegraph-rust SKILL.md ($REF)"
  fetch "$URL" "$src" || die "failed to download $URL"
fi

grep -q '^name: codegraph-rust$' "$src" || die "downloaded file is not a valid codegraph-rust SKILL.md"

install_to() {
  label="$1"
  base_dir="$2"
  dest="$base_dir/codegraph-rust/SKILL.md"
  mkdir -p "$base_dir/codegraph-rust"
  cp "$src" "$dest"
  say "installed [$label] -> $dest"
}

installed=0
if [ -n "$custom_dir" ]; then
  install_to "custom" "$custom_dir"
  installed=$((installed + 1))
fi

if [ -n "$targets" ]; then
  for t in $targets; do
    case "$t" in
      claude) install_to "Claude Code" "$CLAUDE_ROOT/skills" ;;
      codex) install_to "Codex" "$CODEX_ROOT/skills" ;;
      antigravity) install_to "Antigravity" "$ANTIGRAVITY_ROOT/skills" ;;
      cursor) install_to "Cursor" "$CURSOR_ROOT/skills" ;;
      opencode) install_to "OpenCode" "$OPENCODE_ROOT/skills" ;;
    esac
    installed=$((installed + 1))
  done
elif [ -z "$custom_dir" ]; then
  [ -d "$CLAUDE_ROOT" ] && { install_to "Claude Code" "$CLAUDE_ROOT/skills"; installed=$((installed + 1)); }
  [ -d "$CODEX_ROOT" ] && { install_to "Codex" "$CODEX_ROOT/skills"; installed=$((installed + 1)); }
  [ -d "$ANTIGRAVITY_ROOT" ] && { install_to "Antigravity" "$ANTIGRAVITY_ROOT/skills"; installed=$((installed + 1)); }
  [ -d "$CURSOR_ROOT" ] && { install_to "Cursor" "$CURSOR_ROOT/skills"; installed=$((installed + 1)); }
  [ -d "$OPENCODE_ROOT" ] && { install_to "OpenCode" "$OPENCODE_ROOT/skills"; installed=$((installed + 1)); }
  if [ "$installed" -eq 0 ]; then
    install_to "generic (~/.agents)" "$HOME/.agents/skills"
    say "note: no existing agent config dirs detected; pass --claude, --codex, --antigravity, --cursor, --opencode, --all, or --dir <path> to target specific agents"
  fi
fi

if ! command -v cgr >/dev/null 2>&1; then
  say "note: 'cgr' is not on PATH yet; install the binary with:"
  say "      curl -fsSL https://raw.githubusercontent.com/$REPO/main/scripts/install.sh | sh"
fi
