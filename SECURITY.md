# Security policy

Only the latest source revision is supported. Private vulnerability reporting will be enabled before a public release; until then, contact the maintainers privately rather than opening a public issue.

quicKFS is experimental. It assumes a protected persistent server identity, an authenticated out-of-band channel for one-time pairing secrets, strong user passwords, a private network, and an untrusted client confined to one export root. Pairing-assisted certificate pinning and Argon2id password verification are implemented, but server-wide rate limiting, credential lifecycle, recovery, per-user authorization, audit logging, and formal protocol review are incomplete. Do not expose the prototype to the public Internet without understanding these risks.
