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

# Run all tests. Integration tests (tests/smoke.rs) spawn the binary via assert_cmd
# and race on cargo's build lock when parallel — run serial to dodge that.
test:
    cargo build  # pre-build so integration tests don't race on it
    cargo test -- --test-threads=1

# Cargo check + clippy.
lint:
    cargo check
    cargo clippy -- -D warnings
