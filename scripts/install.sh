#!/usr/bin/env bash
# Install worktree-pool + wt into ~/.local/bin/ via symlink.
# Builds the Rust binary first (release profile); the symlink points at the
# stable cargo artifact path, so re-running `cargo build --release` in the
# repo updates the installed tool in place (no re-install step needed).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LINK_DIR="$HOME/.local/bin"
mkdir -p "$LINK_DIR"

echo "building worktree-pool (release profile)…"
cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml"

ln -sfn "$REPO_ROOT/target/release/worktree-pool" "$LINK_DIR/worktree-pool"
ln -sfn "$REPO_ROOT/bin/wt" "$LINK_DIR/wt"
echo "linked $LINK_DIR/worktree-pool → $REPO_ROOT/target/release/worktree-pool"
echo "linked $LINK_DIR/wt → $REPO_ROOT/bin/wt"

if "$LINK_DIR/worktree-pool" doctor >/dev/null 2>&1; then
  echo "doctor passed."
else
  echo "warn: '$LINK_DIR/worktree-pool doctor' did not exit cleanly; run it manually." >&2
fi

case ":$PATH:" in
  *":$LINK_DIR:"*) ;;
  *) echo "warn: $LINK_DIR is not on \$PATH; add to your shell rc:" >&2
     echo "  export PATH=\"\$HOME/.local/bin:\$PATH\"" >&2 ;;
esac
