# Threat model

Clients are untrusted and must not escape the configured root, forge node IDs or handles, exceed resource limits, or use filesystem operations before authentication. The network is untrusted and protected by TLS. The server host, export owner, certificate provisioning, and development token distribution are trusted. Traffic analysis, compromised endpoints, production identity, revocation, and availability attacks remain out of scope for the prototype.

