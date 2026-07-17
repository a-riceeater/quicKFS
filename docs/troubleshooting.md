# Troubleshooting

## The client cannot connect

Confirm the daemon is running and that the client address matches `--bind`:

```sh
RUST_LOG=debug cargo run -p quickfs-server-daemon -- serve ...
cargo run -p quickfs-client-cli -- --server 127.0.0.1:4433 ... ping
```

QUIC uses UDP. Ensure UDP port 4433 is allowed by host and network firewalls. A successful TCP connection test does not verify QUIC reachability.

## TLS handshake or certificate error

For local development, regenerate the certificate and restart the server:

```sh
./scripts/dev-cert.sh
```

Both server and client must use the newly generated `certs/server.crt`; the server must use its matching `certs/server.key`.

The client `--server-name` must appear in the certificate's subject alternative names. The development script creates identities for `localhost` and `127.0.0.1`, while the CLI defaults to `localhost`.

Inspect a certificate:

```sh
openssl x509 -in certs/server.crt -noout -subject -dates -ext subjectAltName
```

Normal code does not provide an insecure certificate-verification bypass.

## Authentication fails

The values passed to the server and client must match exactly:

```sh
# Server
--token development-token

# Client
--token development-token
```

Check whether `QUICKFS_TOKEN` overrides or supplies a value in either terminal. Do not print or paste real credentials into issue reports.

## A path is not found

Remote paths start at the configured export root, not at the server filesystem root. If the server uses:

```text
--export-root /srv/project-share
```

then local `/srv/project-share/example.txt` is remote `/example.txt`.

The path resolver walks directory listings and is case-sensitive when the server filesystem is case-sensitive. Parent traversal such as `..` is not a supported remote navigation mechanism.

## Permission denied while listing

The daemon process needs operating-system permission to traverse directories and read files. A symlink resolving outside the export root is intentionally rejected as a confinement violation.

Check permissions without changing them broadly:

```sh
ls -ld /srv/project-share
ls -l /srv/project-share/example.txt
```

## A ranged read fails

The default maximum request is 8 MiB (`8388608` bytes). Use a smaller `--length` or deliberately adjust the server's `--max-read-size`.

Offsets and lengths are unsigned byte counts. Reads beyond EOF return fewer bytes rather than padding the response.

## Address already in use

Another process may already own UDP port 4433. Stop the other server or select another port on both sides:

```sh
quickfs-server-daemon serve --bind 127.0.0.1:4443 ...
quickfs-client-cli --server 127.0.0.1:4443 ... ping
```

## macFUSE will not mount

Native mounting is not implemented in the current repository. `clients/macos/filesystem-macfuse` is an adapter skeleton only. Use `quickfs-client-cli` to exercise the server without macFUSE.

## Build or test failures

Verify that rustup is honoring the repository toolchain:

```sh
rustup show active-toolchain
rustc --version
```

Then run individual checks to isolate the failure:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo doc --workspace --no-deps
```

When reporting a problem, include the failing command, operating system, Rust version, and sanitized logs.

