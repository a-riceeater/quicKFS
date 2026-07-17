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

The state directory contains the secret server identity and password hashes. Restrict access and back it up securely. CLI arguments also accept documented `QUICKFS_*` environment variables. Ctrl+C and SIGTERM initiate graceful shutdown. A systemd unit is planned, not supplied.
