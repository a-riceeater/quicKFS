#!/usr/bin/env sh
set -eu
cargo fmt --check
# The packaging test suite skips these when they are absent, so run them here
# too rather than letting a missing tool become a silently skipped check.
if command -v shellcheck >/dev/null 2>&1; then
    shellcheck -s sh packaging/install.sh packaging/uninstall.sh packaging/tests/run.sh
fi
if command -v systemd-analyze >/dev/null 2>&1 && [ -x /usr/local/bin/quickfs-server-daemon ]; then
    systemd-analyze verify packaging/systemd/quickfs-server.service
fi
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
