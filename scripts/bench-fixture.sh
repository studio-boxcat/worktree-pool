#!/usr/bin/env bash
# Materialize a tmp bare-repo fixture for end-to-end profiling.
# Echoes the bare repo path on stdout (last line) so callers can capture it.
set -euo pipefail

tmp="${1:-$(mktemp -d)}"
bare="$tmp/source.git"
staging="$tmp/staging"

git init --quiet --bare "$bare"
git clone --quiet "$bare" "$staging"
cd "$staging"
git config user.email t@t
git config user.name t
echo hi > README
git add README
git commit --quiet -m initial
git push --quiet -u origin main 2>/dev/null
echo "$bare"
