# Changelog

## Unreleased

- Fix stuttering large-video playback on mounted volumes: track up to four interleaved sequential streams per handle (macOS multiplexes the player, Spotlight, and Quick Look over one FUSE handle; the old single-stream tracker reset its window on every switch), grow the read-ahead window on demand-read stalls instead of a noisy throughput gradient, bound speculation by bytes *and* by concurrent fetches so the window streams instead of convoying, reserve fetch budgets non-blockingly so unfundable blocks are rescheduled rather than becoming never-fetched holes, deadline speculative fetches so a wedged connection cannot pin the slot pool or hang a joining demand read, serve exactly-1 MiB kernel reads from 1 MiB cache blocks, and mount with `-o noreadahead` because macOS kernel read-ahead was measured racing 100+ MB past a paced consumer, thrashing its own speculative pages, and amplifying reads 5.5× (docs/caching.md). Paced 4–8 MB/s playback from cold data: 18–27 s stalled per minute before, zero stalls after; flat-out cold sequential throughput 26.8 → 49.8 MB/s on the same LAN.

- Initial read-only QUIC filesystem foundation.
- Upgrade the wire protocol to v3 with `quickfs/3` ALPN and mutual role-separated pairing proofs, preventing wrong or unauthenticated pairing attempts from consuming records.
- Enforce the selected TLS trust policy before password prompting, redact/zeroize credential and proof buffers, validate credential sizes, and reject unsafe private-state ownership or layouts.
- Serialize connection login attempts, add per-source throttling and global Argon2/connection/request bounds, and enforce request/client I/O timeouts.
- Scope filesystem capabilities to a connection, cap retained node IDs per connection and globally, release handles on disconnect, enforce the global handle cap atomically, and refuse any export/state directory overlap.
- Add a real loopback QUIC client/server authentication and adversarial-edge test suite.
- Add operating-system/public-PKI and private-CA trust modes, centrally managed pin import, external certificate-chain initialization, and atomically selected identity generations for renewal.
- Remove unneeded/unmaintained transitive dependencies and pass the current RustSec audit with no advisories or warnings.
