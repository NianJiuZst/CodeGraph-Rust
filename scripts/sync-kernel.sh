#!/usr/bin/env bash
# Re-vendor the upstream extraction kernel into crates/cg-kernel (design doc §4.1).
#
#   scripts/sync-kernel.sh <path-to-upstream-git-checkout> [<upstream-sha>]
#
# 1. copies the MVP subset of `codegraph-kernel/` from the given upstream commit
#    (read straight from the object database, the working tree is not touched);
# 2. applies crates/cg-kernel/patches/*.patch in lexical order (P1 no napi +
#    feature-gated languages, P2 allow error trees, P4 parse hook with cached
#    parsers / throttled timeout / tree stash, P5 graph enhancements);
# 3. replaces crates/cg-kernel/{src,grammars,build.rs,Cargo.toml} and records
#    the commit in crates/cg-kernel/UPSTREAM_COMMIT.
#
# Then run `cargo test -p cg-kernel -p cg-extract` (kernel parity tests) before committing.
# A patch that no longer applies must be merged by hand and regenerated with
# scripts/make-kernel-patches.sh.
set -euo pipefail

UP="${1:?usage: sync-kernel.sh <upstream-checkout> [sha]}"
SHA="${2:-$(git -C "$UP" rev-parse HEAD)}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="$ROOT/crates/cg-kernel"
git -C "$UP" cat-file -e "${SHA}^{commit}"
FULL="$(git -C "$UP" rev-parse "$SHA")"

# The subset of codegraph-kernel/ that cgr vendors: the MVP walkers (P1) plus the
# second-phase C/C++, C#, PHP, Ruby and Swift walkers (P5). Dart, Scala, Lua and R
# are not vendored yet.
FILES=(
  Cargo.toml Cargo.lock build.rs
  src/buffers.rs src/ids.rs src/textutil.rs src/docstring.rs src/stack.rs src/langs.rs src/lib.rs
  src/java.rs src/python.rs src/go.rs src/rustlang.rs src/kotlin.rs
  src/tsjs/mod.rs src/tsjs/extractors.rs src/tsjs/fnref.rs
  src/ccpp/mod.rs src/ccpp/brace_scopes.rs src/csharp.rs src/php.rs src/ruby.rs src/swift.rs
  grammars/kotlin/parser.c grammars/kotlin/scanner.c
  grammars/kotlin/tree_sitter/alloc.h grammars/kotlin/tree_sitter/array.h grammars/kotlin/tree_sitter/parser.h
  grammars/swift/LICENSE grammars/swift/parser.c grammars/swift/scanner.c
  grammars/swift/tree_sitter/alloc.h grammars/swift/tree_sitter/array.h grammars/swift/tree_sitter/parser.h
)

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
for f in "${FILES[@]}"; do
  mkdir -p "$TMP/$(dirname "$f")"
  git -C "$UP" show "${FULL}:codegraph-kernel/$f" > "$TMP/$f"
done
for p in "$DEST"/patches/*.patch; do
  echo "applying $(basename "$p")"
  patch -s -d "$TMP" -p1 < "$p"
done

rm -rf "$DEST/src" "$DEST/grammars"
cp -r "$TMP/src" "$DEST/src"
cp -r "$TMP/grammars" "$DEST/grammars"
cp "$TMP/build.rs" "$DEST/build.rs"
cp "$TMP/Cargo.toml" "$DEST/Cargo.toml"
echo "$FULL" > "$DEST/UPSTREAM_COMMIT"
echo "cg-kernel now vendors upstream $FULL"
