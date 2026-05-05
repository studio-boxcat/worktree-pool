#!/usr/bin/env bash
# Symlinks the committed worktree-pool binary into ~/.local/bin/.
# Idempotent: re-runnable safely.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LINK_DIR="$HOME/.local/bin"
mkdir -p "$LINK_DIR"

# Symlinks: <tool> → <repo>/bin/<artifact>
declare -a LINKS=(
  "worktree-pool:$REPO_ROOT/bin/worktree-pool-darwin-arm64"
  "worktree-pool-session:$REPO_ROOT/bin/worktree-pool-session"
)

if [ "$(uname -sm)" != "Darwin arm64" ]; then
  echo "warn: committed Rust binary is darwin-arm64; you're on $(uname -sm)." >&2
fi

for entry in "${LINKS[@]}"; do
  name="${entry%%:*}"
  src="${entry#*:}"
  dst="$LINK_DIR/$name"
  if [ ! -x "$src" ]; then
    echo "missing or non-executable: $src" >&2
    [ "$name" = "worktree-pool" ] && echo "  rebuild via: just release-binary" >&2
    exit 1
  fi
  ln -sfn "$src" "$dst"
  echo "linked $dst → $src"
done

# Sanity-check the Rust binary; the bash script doesn't need exercising.
if "$LINK_DIR/worktree-pool" doctor >/dev/null 2>&1; then
  echo "doctor passed."
else
  echo "warn: '$LINK_DIR/worktree-pool doctor' did not exit cleanly; run it manually to inspect." >&2
fi

case ":$PATH:" in
  *":$LINK_DIR:"*) ;;
  *) echo "warn: $LINK_DIR is not on \$PATH; add to your shell rc:" >&2
     echo "  export PATH=\"\$HOME/.local/bin:\$PATH\"" >&2 ;;
esac
