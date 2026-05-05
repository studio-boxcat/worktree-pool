#!/usr/bin/env bash
# Symlinks the committed worktree-pool binary into ~/.local/bin/.
# Idempotent: re-runnable safely.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_SRC="$REPO_ROOT/bin/worktree-pool-darwin-arm64"
LINK_DIR="$HOME/.local/bin"
LINK_DST="$LINK_DIR/worktree-pool"

if [ ! -x "$BIN_SRC" ]; then
  echo "missing or non-executable: $BIN_SRC" >&2
  echo "rebuild via: just release-binary" >&2
  exit 1
fi

if [ "$(uname -sm)" != "Darwin arm64" ]; then
  echo "warn: committed binary is darwin-arm64; you're on $(uname -sm). Tool may not run." >&2
fi

mkdir -p "$LINK_DIR"
ln -sfn "$BIN_SRC" "$LINK_DST"
echo "linked $LINK_DST → $BIN_SRC"

# Sanity: try a doctor invocation to exercise the symlink.
if "$LINK_DST" doctor >/dev/null 2>&1; then
  echo "doctor passed."
else
  echo "warn: '$LINK_DST doctor' did not exit cleanly; run it manually to inspect." >&2
fi

# PATH hint if ~/.local/bin isn't in PATH.
case ":$PATH:" in
  *":$LINK_DIR:"*) ;;
  *) echo "warn: $LINK_DIR is not on \$PATH; add to your shell rc:" >&2
     echo "  export PATH=\"\$HOME/.local/bin:\$PATH\"" >&2 ;;
esac
