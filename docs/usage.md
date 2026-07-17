# Usage and command reference

The current user-facing tools are the Linux server daemon and a platform-independent diagnostic client. The client can browse and read the export without macFUSE.

## Server command

```text
quickfs-server-daemon serve [OPTIONS] \
  --export-root <PATH> --cert <PATH> --key <PATH> --token <TOKEN>
```

Options:

| Option | Environment variable | Default | Purpose |
| --- | --- | --- | --- |
| `--bind <ADDRESS>` | `QUICKFS_BIND` | `0.0.0.0:4433` | UDP address on which QUIC listens. |
| `--export-root <PATH>` | `QUICKFS_EXPORT_ROOT` | Required | Directory exposed as the remote root. |
| `--cert <PATH>` | `QUICKFS_CERT` | Required | PEM server certificate. |
| `--key <PATH>` | `QUICKFS_KEY` | Required | PEM private key. |
| `--token <TOKEN>` | `QUICKFS_TOKEN` | Required | Experimental shared development token. |
| `--max-read-size <BYTES>` | — | `8388608` | Largest permitted ranged read (8 MiB). |
| `--max-open-handles <COUNT>` | — | `1024` | Maximum tracked open files. |
| `--request-timeout-ms <MS>` | — | `30000` | Configured request timeout value; enforcement is not complete. |
| `--max-concurrent-requests <COUNT>` | — | `128` | Global request concurrency bound. |

Example using flags:

```sh
RUST_LOG=info quickfs-server-daemon serve \
  --bind 0.0.0.0:4433 \
  --export-root /srv/project-share \
  --cert /etc/quickfs/server.crt \
  --key /etc/quickfs/server.key \
  --token development-token \
  --max-read-size 8388608 \
  --max-open-handles 1024 \
  --max-concurrent-requests 128
```

Example using environment variables:

```sh
export QUICKFS_BIND=0.0.0.0:4433
export QUICKFS_EXPORT_ROOT=/srv/project-share
export QUICKFS_CERT=/etc/quickfs/server.crt
export QUICKFS_KEY=/etc/quickfs/server.key
export QUICKFS_TOKEN=development-token
RUST_LOG=quickfs_server_daemon=debug quickfs-server-daemon serve
```

Avoid putting real credentials in shell history. The token mechanism is only a development placeholder. Optional TOML configuration is planned but not currently implemented.

## Client global options

```text
quickfs-client-cli [OPTIONS] --cert <PATH> --token <TOKEN> <COMMAND>
```

| Option | Environment variable | Default | Purpose |
| --- | --- | --- | --- |
| `--server <ADDRESS>` | `QUICKFS_SERVER` | `127.0.0.1:4433` | Server socket address. |
| `--server-name <NAME>` | — | `localhost` | DNS identity validated against the TLS certificate. |
| `--cert <PATH>` | `QUICKFS_CERT` | Required | Explicitly trusted server certificate. |
| `--token <TOKEN>` | `QUICKFS_TOKEN` | Required | Token sent in the authentication request. |
| `--timeout-ms <MS>` | — | `30000` | Connection and stream-open timeout. |

The server name is a certificate identity, not necessarily the address used to reach the server. For example, a client can connect to `192.0.2.10:4433` while validating a certificate issued for `files.example.net`:

```sh
quickfs-client-cli \
  --server 192.0.2.10:4433 \
  --server-name files.example.net \
  --cert ./files.example.net.crt \
  --token development-token \
  ping
```

## `ping`

Checks QUIC, TLS, protocol framing, and basic request/response handling:

```sh
quickfs-client-cli --cert ./certs/server.crt --token development-token ping
```

Expected output:

```text
pong 42
```

Ping is currently allowed before server-side filesystem authentication checks, although the CLI establishes an authenticated client first.

## `list`

Lists one remote directory:

```sh
quickfs-client-cli --cert ./certs/server.crt --token development-token list /
quickfs-client-cli --cert ./certs/server.crt --token development-token list /examples
```

Example output:

```text
Directory	examples
File	hello.txt
```

Paths are resolved client-side by walking directory entries. Only opaque node IDs travel over the network.

## `stat`

Prints metadata for a file or directory:

```sh
quickfs-client-cli --cert ./certs/server.crt --token development-token stat /
quickfs-client-cli --cert ./certs/server.crt --token development-token stat /hello.txt
```

The debug-style output includes node ID, kind, size, revision, and modification time in Unix milliseconds. Revisions are change indicators, not globally ordered version numbers.

## `read`

Opens a file, reads a byte range, writes the raw data to stdout, and closes the handle:

```sh
quickfs-client-cli --cert ./certs/server.crt --token development-token \
  read /hello.txt --offset 0 --length 4096
```

Read a middle range:

```sh
quickfs-client-cli --cert ./certs/server.crt --token development-token \
  read /archive.bin --offset 1048576 --length 65536 > block.bin
```

Important behavior:

- `--length` is required and expressed in bytes.
- `--offset` defaults to zero.
- A read extending beyond EOF returns only the available bytes.
- A zero-length read succeeds with no output.
- The server rejects reads over its configured maximum.
- Output is raw file content; redirect it when reading binary data.

## Logging

Set `RUST_LOG` when running the server:

```sh
RUST_LOG=info quickfs-server-daemon serve ...
RUST_LOG=quickfs_server_daemon=debug quickfs-server-daemon serve ...
RUST_LOG=warn quickfs-server-daemon serve ...
```

Logs should not contain tokens or file contents. Remove sensitive addresses and paths before sharing diagnostic output.

## Current operational limits

- One long-lived QUIC connection is used per CLI invocation; the CLI exits after one command.
- Each filesystem request uses an independent bidirectional QUIC stream.
- The implementation is read-only.
- Native macOS mounting is not implemented, so there is no supported `mount` command.
- Reconnect and automatic retry behavior are incomplete.
- Server reads are bounded but currently buffered before being sent.
- The development token is not production authentication.
- Do not expose the prototype directly to the public Internet.

