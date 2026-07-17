# Filesystem semantics

Version 1 is read-only. Directories and regular files support lookup, metadata, listing, open, ranged read, and close. Reads past EOF return fewer bytes; zero-length reads succeed. Server revisions are change indicators, not globally ordered versions. Symlinks resolving outside the export are rejected. Writes, locks, xattrs, hard-link identity, notifications, and offline behavior are unsupported.

