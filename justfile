# Build a reproducible-ish, ad-hoc-codesigned release binary at bin/worktree-pool-darwin-arm64.
# `--remap-path-prefix` flattens absolute build paths so binary diffs are stable across machines.
release-binary:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "$(uname -sm)" != "Darwin arm64" ]; then
        echo "release-binary only builds the macOS arm64 artifact; refusing on $(uname -sm)" >&2
        exit 1
    fi
    RUSTFLAGS="--remap-path-prefix=$PWD=." cargo build --release
    mkdir -p bin
    cp target/release/worktree-pool bin/worktree-pool-darwin-arm64
    codesign --sign - --force bin/worktree-pool-darwin-arm64
    ls -lh bin/worktree-pool-darwin-arm64
    file bin/worktree-pool-darwin-arm64
    echo "ok — committed binary updated. Don't forget to commit bin/worktree-pool-darwin-arm64."

# Symlink committed binaries into ~/.local/bin/.
install:
    scripts/install.sh

# Run all tests. Integration tests (tests/smoke.rs) spawn the binary via assert_cmd
# and race on cargo's build lock when parallel — run serial to dodge that.
test:
    cargo build  # pre-build so integration tests don't race on it
    cargo test -- --test-threads=1

# Cargo check + clippy.
lint:
    cargo check
    cargo clippy -- -D warnings

# Microbench pure-Rust hot paths (YAML parse/serialize). Criterion-based.
bench:
    cargo bench

# End-to-end CLI timing via hyperfine. Measures acquire+release wall-clock against a tmp bare-repo
# fixture. Prereq: hyperfine (brew install hyperfine).
bench-cli:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release
    BIN="$(pwd)/target/release/worktree-pool"
    TMP=$(mktemp -d)
    KEY="bench-$(date +%s)"
    POOL="$WORKTREE_ROOT/$KEY"
    trap "rm -rf '$TMP' '$POOL'" EXIT
    BARE=$(scripts/bench-fixture.sh "$TMP")
    "$BIN" --pool "$KEY" init --source "$BARE" --max-slots 4 --groups ios
    echo
    echo "==> acquire (cold + warm)"
    hyperfine --warmup 1 --runs 5 \
      --prepare "'$BIN' --pool '$KEY' release --name foo 2>/dev/null || true" \
      "'$BIN' --pool '$KEY' acquire --name foo --commit main --group ios"
    echo
    echo "==> release"
    "$BIN" --pool "$KEY" acquire --name foo --commit main --group ios >/dev/null
    hyperfine --warmup 1 --runs 5 \
      --prepare "'$BIN' --pool '$KEY' acquire --name foo --commit main --group ios >/dev/null 2>&1 || true" \
      "'$BIN' --pool '$KEY' release --name foo"
    echo
    echo "==> ls (no held slots)"
    hyperfine --warmup 1 --runs 10 "'$BIN' --pool '$KEY' ls"
    echo
    echo "==> ls --git-status (1 held slot)"
    "$BIN" --pool "$KEY" acquire --name foo --commit main --group ios >/dev/null
    hyperfine --warmup 1 --runs 5 "'$BIN' --pool '$KEY' ls --git-status"

# Capture an acquire+release sampling profile via samply. Opens in Firefox profiler UI.
# Prereq: samply (cargo install samply).
profile:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release
    BIN="$(pwd)/target/release/worktree-pool"
    TMP=$(mktemp -d)
    KEY="profile-$(date +%s)"
    POOL="$WORKTREE_ROOT/$KEY"
    trap "rm -rf '$TMP' '$POOL'" EXIT
    BARE=$(scripts/bench-fixture.sh "$TMP")
    "$BIN" --pool "$KEY" init --source "$BARE" --max-slots 4 --groups ios
    echo "==> profiling acquire (cold)"
    samply record -- "$BIN" --pool "$KEY" acquire --name foo --commit main --group ios
