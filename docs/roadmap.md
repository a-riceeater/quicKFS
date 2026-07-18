# Roadmap

1. Add recovery/live-session revocation, per-user authorization, distributed-login defenses, and an independent formal authentication review. Automated loopback pairing/login/QUIC tests, per-source throttling, and global authentication-work limits are implemented.
2. Harden the implemented read-only macFUSE mount with installed-extension integration and fault-injection CI tests.
3. Add reconnect, idempotent retry, and bounded read streaming.
4. Add persistent metadata and disk-backed range caching.
5. Harden observability, fuzzing, recovery, old-identity-signed rotation for exact-pin deployments, QR pairing, and deployment (including systemd). Atomic CA-backed identity rotation is implemented.
6. Design writes and WinFsp support; neither is currently implemented.
