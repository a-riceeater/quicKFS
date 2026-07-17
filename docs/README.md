# quicKFS documentation

quicKFS is currently an experimental read-only prototype. Start with the setup guide, then use the command reference to run and inspect a local server.

- [Setup](setup.md): prerequisites, source build, identity initialization, pairing, and first local session.
- [Usage and command reference](usage.md): server and client commands, environment variables, and operational behavior.
- [Authentication and server trust](authentication.md): pairing-assisted TOFU, certificate pins, user passwords, and connection authentication.
- [Development](development.md): repository workflow, quality checks, and common development tasks.
- [Troubleshooting](troubleshooting.md): TLS, connectivity, authentication, and filesystem errors.
- [Protocol](protocol.md): version 2 wire format and request model.
- [Filesystem semantics](filesystem-semantics.md): current read-only behavior.
- [Architecture](../ARCHITECTURE.md): subsystem and trust boundaries.
- [Threat model](threat-model.md): current security assumptions.
- [Caching](caching.md): implemented interfaces and planned cache work.
- [Roadmap](roadmap.md): planned milestones.

Native macFUSE mounting, write support, persistent caching, and production authentication are not yet implemented.
