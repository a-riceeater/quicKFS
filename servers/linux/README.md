# Linux server

Generate a certificate, then run:

```sh
cargo run -p quickfs-server-daemon -- serve --bind 0.0.0.0:4433 --export-root /srv/project-share --cert ./certs/server.crt --key ./certs/server.key --token development-token
```

Arguments also accept the documented `QUICKFS_*` environment variables. Set `RUST_LOG=info` for logs. Ctrl+C and SIGTERM initiate graceful shutdown. A systemd unit is planned, not supplied.

