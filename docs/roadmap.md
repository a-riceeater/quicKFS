# Roadmap

Implemented foundations include authenticated protocol-v6 reads/writes, one-request enriched native directory views, per-account write grants, persistent node identity, reconnect/lock replay, disk-backed offline reads, broad macFUSE callbacks, xattrs/resource forks, hardlinks/special nodes, server-side copies, data/hole seek, and mounted-volume integration tests.

Remaining priorities:

1. Recovery, immediate live-session revocation, per-user export roots, stronger distributed-login defense, audit logging, and an independent protocol/security review.
2. Fault-injection and Linux/macOS CI for daemon restart, macFUSE 4/5 backends, backing-filesystem variants, special nodes, and large resource forks.
3. Server-pushed cross-client cache invalidation plus better observability/profiling for multi-client media workloads. Local post-mutation projections are already invalidated coherently; synchronous fuser notifications from inside macOS callbacks are unsafe with the one receive loop.
4. Signed/overlapping exact-pin rotation, platform keychain integration, deployment packaging/systemd, and administrator recovery tooling.
5. WinFsp support.

Cold-start offline mounting and offline writes are intentionally deferred unless a future design includes authenticated durable journals, version preconditions, lock/permission revalidation, explicit conflict policy, and user-visible reconciliation.
