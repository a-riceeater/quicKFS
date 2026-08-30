# Windows client

Two crates:

- [`filesystem-winfsp`](filesystem-winfsp) — the WinFsp bridge. It translates
  WinFsp's path-addressed callbacks into operations on the shared
  [`quickfs-fs-adapter`](../../crates/fs-adapter), maps quicKFS errors onto the
  `NTSTATUS` values Windows applications branch on, and owns the mount
  lifecycle. Everything protocol-related — caching, reconnect, write
  coalescing, revision checking — is the same code the macOS client runs.
- [`client-cli`](client-cli) — the `quickfs` executable: `mount`, `unmount`,
  `pair`, `forget` and `trust`.

Start with [the Windows client guide](../../docs/windows.md) for requirements,
building, mounting, the POSIX-to-Windows metadata mapping, the limitations, and
troubleshooting. The manual coverage that needs a mounted volume is in
[the integration test plan](../../docs/windows-integration-tests.md).
