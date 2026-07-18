# Filesystem semantics

Protocol version 3 remains read-only. Directories and regular files support lookup, metadata, listing, open, ranged read, and close. Reads past EOF return fewer bytes; zero-length reads succeed. Server revisions are change indicators, not globally ordered versions. Symlinks resolving outside the export are rejected; safe in-export symlinks are exposed by the macFUSE adapter as their resolved target because version 3 has no `readlink` operation.

The macOS mount reports directories as mode `0555` and files as `0444`, disables device/set-ID/executable semantics, and exposes no extended attributes. Finder receives stable local inode numbers for opaque remote nodes and local file handles that are closed through the remote session on `release`. Writes, locks, hard-link identity, notifications, reconnect/offline behavior, and persistent caching are unsupported.
