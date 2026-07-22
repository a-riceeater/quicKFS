# Contributing

Install the pinned stable Rust toolchain, then run `./scripts/check.sh`. Pull requests should be focused, explain behavior changes, include tests, and keep commits reviewable. Format with rustfmt and resolve Clippy warnings.

Reusable crates belong in `crates/`; platform code belongs under `clients/` or `servers/`. Protocol changes must update `docs/protocol.md` and the formal wire spec `docs/protocol-spec.md`, preserve explicit version handling, add round-trip and rejection tests, and describe compatibility impact. Do not introduce raw paths into the wire protocol. Before changing the wire format, follow the versioning guideline in `docs/protocol-spec.md` §8.2 — Postcard is positional and non-self-describing, so a compatible **minor** may only *append* enum variants (never insert, reorder, remove, or reshape a struct) and must gate emission on negotiation; anything else is a **major** (flag-day) bump.

Report bugs through the issue templates. Include platform, Rust version, logs with secrets removed, and reproduction steps. macOS adapter changes should describe the tested macFUSE version; Linux server changes should test confinement. Never commit passwords, pairing codes, trust databases, private keys, fixture secrets, or file contents from users.
