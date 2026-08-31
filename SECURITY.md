# Security policy

Only the latest source revision is supported. Private vulnerability reporting will be enabled before a public release; until then, contact the maintainers privately rather than opening a public issue.

quicKFS is experimental. It assumes protected server identity/export state, strong user passwords, and an untrusted client confined to one export root. The selected bootstrap must also be protected: the pairing-code channel, centrally distributed exact pin, deployed enterprise CA, or operating-system trust policy. Mutual-proof pairing, managed pins, standard X.509 validation, atomic identity generations, Argon2id verification, per-account write grants, descriptor-relative confinement, request/resource limits, and fail-closed offline mutation behavior are implemented. Credential recovery, live-session revocation, distributed-login defense, per-user export roots, audit logging, revocation integration, and independent formal protocol review remain incomplete. Do not expose the prototype to the public Internet without understanding these risks.

## Deployment assumptions

The supported Linux deployment runs the daemon as the dedicated `quickfs` system
account: no password, no interactive shell, no created home directory, no
supplementary groups. That account should own its private state directory
(`/var/lib/quickfs`, mode `0700`) and nothing else — anything it can read or
write is reachable through a compromise of the network-facing daemon, so giving
it ownership of unrelated data converts one service bug into access to that
data. The configuration (`/etc/quickfs/quickfs.toml`, `root:quickfs`, mode
`0640`) is readable but not writable by the service; the daemon refuses a
configuration file that is a symbolic link or is writable by group or other
users, because anything able to rewrite it chooses the export root and the state
directory.

The supplied systemd unit runs with no capabilities at all, which also removes
`CAP_MKNOD` and therefore character- and block-device creation. Restoring it is
a documented expert override with a stated cost: an authenticated client that
can create a writable device node in a directory another process trusts has a
privilege-escalation primitive. `ProtectHome=`, `ProtectSystem=strict` and
`PrivateUsers=` are deliberately omitted because each would break a legitimate
administrator-selected export rather than protect one; the reasoning is recorded
in `servers/linux/README.md`.

Release archives are published with SHA-256 checksums from the same GitHub
release. Those detect corruption and a mirror that altered the bytes; they are
**not** an independent signature trust chain, because whoever could replace the
archive could replace the checksum beside it. Detached cryptographic release
signing is not yet configured for this project. The installer verifies before it
extracts or executes anything, and is structured so a signature check can be
added in front of that step.

Losing `/var/lib/quickfs` is unrecoverable: it destroys the server identity
(every exact-pin client must be paired again) and the export epoch (every client
discards its cache and reconnects as if to a different server). Back it up as
securely as the private key it contains.
