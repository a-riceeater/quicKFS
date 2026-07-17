# Roadmap

1. Add automated pairing/login/QUIC integration tests, server-wide rate limits, recovery/live-session revocation, per-user authorization, and formal authentication review.
2. Complete native read-only macFUSE callbacks and fault-injection CI tests.
3. Add reconnect, idempotent retry, and bounded read streaming.
4. Add persistent metadata and disk-backed range caching.
5. Harden observability, fuzzing, recovery, signed identity rotation, QR pairing, and deployment (including systemd).
6. Design writes and WinFsp support; neither is currently implemented.
