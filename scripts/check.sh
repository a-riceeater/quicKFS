#!/usr/bin/env sh
set -eu
cargo fmt --check
if [ "$(uname -s)" = "Darwin" ] && \
    { ! command -v pkg-config >/dev/null 2>&1 || ! pkg-config --exists fuse; }
then
    # Compile the entire callback/API surface even on development Macs where
    # the separately installed macFUSE SDK is intentionally absent.
    cargo clippy --workspace --all-targets --all-features \
        --features fuser/macos-no-mount -- -D warnings
else
    cargo clippy --workspace --all-targets --all-features -- -D warnings
fi
cargo test --workspace
cargo doc --workspace --no-deps
