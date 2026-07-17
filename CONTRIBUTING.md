# Contributing

Install the pinned stable Rust toolchain, then run `./scripts/check.sh`. Pull requests should be focused, explain behavior changes, include tests, and keep commits reviewable. Format with rustfmt and resolve Clippy warnings.

Reusable crates belong in `crates/`; platform code belongs under `clients/` or `servers/`. Protocol changes must update `docs/protocol.md`, preserve explicit version handling, add round-trip and rejection tests, and describe compatibility impact. Do not introduce raw paths into the wire protocol.

Report bugs through the issue templates. Include platform, Rust version, logs with secrets removed, and reproduction steps. macOS adapter changes should describe the tested macFUSE version; Linux server changes should test confinement. Never commit passwords, pairing codes, trust databases, private keys, fixture secrets, or file contents from users.
