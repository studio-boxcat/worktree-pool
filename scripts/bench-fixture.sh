#!/usr/bin/env bash
# Materialize a tmp bare-repo fixture for end-to-end profiling. Echoes the
# bare repo path on stdout (last line) so callers can capture it.
#
# Args:
#   $1  optional tmpdir (default: mktemp -d)
#   $2  optional N submodules (default: 0). When N > 0 the source registers N
#       independent submodule bares — exercises submodules.rs's parallel
#       phase 2 + the per-source config mutex. Without submodules the cold-
#       acquire wall time is unrealistically low (no clone fan-out).
set -euo pipefail

tmp="${1:-$(mktemp -d)}"
n_subs="${2:-0}"
bare="$tmp/source.git"
staging="$tmp/staging"

mk_commit() {
  local dir="$1" file="$2" content="$3" msg="$4"
  echo "$content" > "$dir/$file"
  git -C "$dir" add "$file"
  git -C "$dir" -c user.email=t@t -c user.name=t commit --quiet -m "$msg"
}

git init --quiet --bare "$bare"
git clone --quiet "$bare" "$staging"
mk_commit "$staging" README "hi" initial

if [ "$n_subs" -gt 0 ]; then
  # Build N submodule bares + register them in the parent.
  for i in $(seq 0 $((n_subs - 1))); do
    sub_bare="$tmp/sub-$i.git"
    sub_staging="$tmp/sub-$i-staging"
    git init --quiet --bare "$sub_bare"
    git clone --quiet "$sub_bare" "$sub_staging"
    mk_commit "$sub_staging" FILE "sub-$i-content" "sub $i init"
    git -C "$sub_staging" push --quiet -u origin main 2>/dev/null
    git -C "$staging" -c protocol.file.allow=always \
      submodule add --quiet "$sub_bare" "sub-$i"
  done
  git -C "$staging" -c user.email=t@t -c user.name=t \
    commit --quiet -m "add $n_subs submodules"
fi

git -C "$staging" push --quiet -u origin main 2>/dev/null
echo "$bare"
