# Windows quality gate, matching scripts/check.sh.
#
# The workspace builds on Windows, but the server crates are largely
# `cfg(unix)`, so most of their tests report NotSupported here and their
# Linux-only code shows up as dead. That is why this script checks and tests the
# client half; the server half is covered on Linux by CI and by
# scripts/check.sh.
#
# Building the Windows client needs WinFsp's import library. Set
# WINFSP_INSTALL_DIR to an SDK root (the directory holding inc\ and lib\) when
# WinFsp was installed without its Developer feature.

$ErrorActionPreference = 'Stop'

cargo fmt --check
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Scoped to what Windows builds: a whole-workspace lint would fail on dead-code
# warnings for the cfg(unix) server crates, which check.sh and Linux CI cover.
cargo clippy --all-targets `
  -p quickfs-protocol `
  -p quickfs-auth `
  -p quickfs-cache `
  -p quickfs-common `
  -p quickfs-client-core `
  -p quickfs-fs-adapter `
  -p quickfs-transport-quic `
  -p quickfs-windows-support `
  -p quickfs-filesystem-winfsp `
  -p quickfs-windows-cli `
  -p quickfs-filesystem-macfuse `
  -p quickfs-client-cli `
  -- -D warnings
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

cargo test `
  -p quickfs-protocol `
  -p quickfs-auth `
  -p quickfs-cache `
  -p quickfs-common `
  -p quickfs-client-core `
  -p quickfs-fs-adapter `
  -p quickfs-transport-quic `
  -p quickfs-windows-support `
  -p quickfs-filesystem-winfsp `
  -p quickfs-windows-cli
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

cargo doc --workspace --no-deps
exit $LASTEXITCODE
