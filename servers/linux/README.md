# Linux server

Initialize identity and add a user once:

```sh
quickfs-server-daemon init --state-dir /var/lib/quickfs --server-name files.example.net
quickfs-server-daemon user add --state-dir /var/lib/quickfs alice
```

Run the server:

```sh
RUST_LOG=info quickfs-server-daemon serve \
  --bind 0.0.0.0:4433 \
  --export-root /srv/project-share \
  --state-dir /var/lib/quickfs
```

Create a one-time client pairing:

```sh
quickfs-server-daemon pair create --state-dir /var/lib/quickfs
```

For managed deployments, `init --certificate <FULLCHAIN.pem> --private-key <KEY.pem>` imports an externally issued identity, and `identity install` atomically selects a validated renewal for the next daemon start. Clients can use a deployed private CA, their operating-system trust policy, or a centrally imported exact pin instead of pairing. See the main setup and authentication guides.

The state directory contains the secret server identity, password hashes, and pairing records. It and the export root must be disjoint directory trees; on Unix the daemon requires mode `0700` directories and `0600` private files. Restrict access and back it up securely. CLI arguments also accept documented `QUICKFS_*` environment variables. Ctrl+C and SIGTERM initiate graceful shutdown. A systemd unit is planned, not supplied.
