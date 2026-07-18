# quicKFS documentation

quicKFS is currently an experimental read-only prototype. Start with the setup guide, then use the command reference to run and inspect a local server.

- [Setup](setup.md): local pairing plus enterprise CA, platform-root, and managed-pin deployment.
- [Usage and command reference](usage.md): server and client commands, environment variables, and operational behavior.
- [Authentication and server trust](authentication.md): pairing-assisted exact pins, enterprise PKI, user passwords, and connection authentication.
- [Development](development.md): repository workflow, quality checks, and common development tasks.
- [Troubleshooting](troubleshooting.md): TLS, connectivity, authentication, and filesystem errors.
- [Protocol](protocol.md): version 3 wire format and request model.
- [Filesystem semantics](filesystem-semantics.md): current read-only behavior.
- [Architecture](../ARCHITECTURE.md): subsystem and trust boundaries.
- [Threat model](threat-model.md): current security assumptions.
- [Caching](caching.md): implemented interfaces and planned cache work.
- [Roadmap](roadmap.md): planned milestones.

Native read-only macFUSE mounting is available behind the `macfuse` feature. Reconnect, write support, persistent caching, per-user authorization, and production hardening are not yet implemented.
