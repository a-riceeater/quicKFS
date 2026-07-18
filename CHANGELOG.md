# Changelog

## Unreleased

- Initial read-only QUIC filesystem foundation.
- Upgrade the wire protocol to v3 with `quickfs/3` ALPN and mutual role-separated pairing proofs, preventing wrong or unauthenticated pairing attempts from consuming records.
- Enforce the selected TLS trust policy before password prompting, redact/zeroize credential and proof buffers, validate credential sizes, and reject unsafe private-state ownership or layouts.
- Serialize connection login attempts, add per-source throttling and global Argon2/connection/request bounds, and enforce request/client I/O timeouts.
- Scope filesystem capabilities to a connection, cap retained node IDs per connection and globally, release handles on disconnect, enforce the global handle cap atomically, and refuse any export/state directory overlap.
- Add a real loopback QUIC client/server authentication and adversarial-edge test suite.
- Add operating-system/public-PKI and private-CA trust modes, centrally managed pin import, external certificate-chain initialization, and atomically selected identity generations for renewal.
- Remove unneeded/unmaintained transitive dependencies and pass the current RustSec audit with no advisories or warnings.
