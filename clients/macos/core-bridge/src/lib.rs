// SPDX-License-Identifier: Apache-2.0
//! The native macOS app's bridge into `quickfs-client-core`.
//!
//! The app links this crate as a static library and calls one C function
//! with a JSON request; the reply is `{"ok": ...}` or
//! `{"error": {"code", "message"}}`. Keeping the ABI to strings means the
//! Swift side never handles Rust-owned structs, and every operation reuses
//! exactly the trust, authentication, reconnect, and filesystem code the CLI
//! and mount use. Calls block the calling thread, so the app issues them off
//! the main thread; long transfers return immediately with an id to poll.

mod ffi;
mod model;
mod session;
mod transfer;

pub use ffi::{quickfs_bridge_call, quickfs_bridge_free};

use model::{BridgeError, BridgeResult, Endpoint, TrustSpec, node_id};
use quickfs_client_core::{
    MAX_CLIENT_READ_SIZE, MAX_CLIENT_WRITE_SIZE, TrustStore, parse_sha256_fingerprint,
    verify_pairing,
};
use quickfs_transport_quic::{PairingClient, certificate_sha256, load_certificates};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::OnceLock, time::Duration};
use uuid::Uuid;
use zeroize::Zeroizing;

const DEFAULT_PORT: u16 = 4433;
const PAIRING_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    Environment,
    Resolve {
        host: String,
    },
    TrustList {
        state_dir: PathBuf,
    },
    Pair {
        state_dir: PathBuf,
        server: Endpoint,
        pairing_id: Uuid,
        code: Zeroizing<String>,
    },
    Forget {
        state_dir: PathBuf,
        server: Endpoint,
    },
    TrustImport {
        state_dir: PathBuf,
        server: Endpoint,
        #[serde(default)]
        sha256: Option<String>,
        #[serde(default)]
        certificate: Option<PathBuf>,
    },
    Connect {
        state_dir: PathBuf,
        server: Endpoint,
        trust: TrustSpec,
        username: String,
        password: Zeroizing<String>,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    Disconnect {
        session: u64,
    },
    Ping {
        session: u64,
    },
    Capabilities {
        session: u64,
    },
    FilesystemStats {
        session: u64,
    },
    List {
        session: u64,
        node: Uuid,
    },
    Stat {
        session: u64,
        node: Uuid,
    },
    ResolvePath {
        session: u64,
        path: String,
    },
    ReadLink {
        session: u64,
        node: Uuid,
    },
    Read {
        session: u64,
        node: Uuid,
        #[serde(default)]
        offset: u64,
        length: u64,
    },
    CreateDirectory {
        session: u64,
        parent: Uuid,
        name: String,
        #[serde(default)]
        raw_name: Option<String>,
        #[serde(default = "default_directory_mode")]
        mode: u32,
    },
    CreateFile {
        session: u64,
        parent: Uuid,
        name: String,
        #[serde(default)]
        raw_name: Option<String>,
        #[serde(default = "default_file_mode")]
        mode: u32,
    },
    Remove {
        session: u64,
        parent: Uuid,
        name: String,
        #[serde(default)]
        raw_name: Option<String>,
        #[serde(default)]
        directory: bool,
        #[serde(default)]
        recursive: bool,
    },
    Rename {
        session: u64,
        parent: Uuid,
        name: String,
        #[serde(default)]
        raw_name: Option<String>,
        new_parent: Uuid,
        new_name: String,
        #[serde(default)]
        no_replace: bool,
    },
    Truncate {
        session: u64,
        node: Uuid,
        size: u64,
    },
    SetMode {
        session: u64,
        node: Uuid,
        mode: u32,
    },
    Download {
        session: u64,
        node: Uuid,
        destination: PathBuf,
        #[serde(default)]
        overwrite: bool,
    },
    Upload {
        session: u64,
        parent: Uuid,
        source: PathBuf,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        replace: bool,
    },
    WriteAt {
        session: u64,
        node: Uuid,
        source: PathBuf,
        #[serde(default)]
        offset: u64,
        #[serde(default)]
        sync: bool,
    },
    TransferStatus {
        transfer: u64,
    },
    TransferCancel {
        transfer: u64,
    },
    TransferForget {
        transfer: u64,
    },
}

const fn default_directory_mode() -> u32 {
    0o755
}

const fn default_file_mode() -> u32 {
    0o644
}

fn runtime() -> BridgeResult<&'static tokio::runtime::Runtime> {
    static RUNTIME: OnceLock<Result<tokio::runtime::Runtime, String>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_name("quickfs-bridge")
                .build()
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(|error| BridgeError::invalid(format!("failed to start the runtime: {error}")))
}

/// Handle one JSON request and return the JSON reply. This is the safe core
/// of the C ABI and is what the tests exercise.
pub fn call_json(request: &str) -> String {
    let reply = match serde_json::from_str::<Request>(request) {
        Ok(request) => runtime().and_then(|runtime| runtime.block_on(dispatch(runtime, request))),
        Err(error) => Err(BridgeError::invalid(format!("malformed request: {error}"))),
    };
    let value = match reply {
        Ok(value) => json!({ "ok": value }),
        Err(error) => json!({ "error": { "code": error.code(), "message": error.to_string() } }),
    };
    value.to_string()
}

async fn dispatch(
    runtime: &'static tokio::runtime::Runtime,
    request: Request,
) -> BridgeResult<Value> {
    match request {
        Request::Environment => Ok(json!({
            "macfuse_installed": quickfs_macos_support::macfuse_is_installed(),
            "macfuse_install_url": quickfs_macos_support::MACFUSE_INSTALL_URL,
            "max_client_read_size": MAX_CLIENT_READ_SIZE,
            "max_client_write_size": MAX_CLIENT_WRITE_SIZE,
            "core_version": env!("CARGO_PKG_VERSION"),
        })),
        Request::Resolve { host } => resolve(&host).await,
        Request::TrustList { state_dir } => {
            let pins: Vec<Value> = TrustStore::open(&state_dir)?
                .list()?
                .into_iter()
                .map(|pin| {
                    json!({
                        "address": pin.address.to_string(),
                        "server_name": pin.server_name,
                        "sha256": hex::encode(pin.certificate_sha256),
                    })
                })
                .collect();
            Ok(json!({ "servers": pins }))
        }
        Request::Pair {
            state_dir,
            server,
            pairing_id,
            code,
        } => pair(state_dir, server, pairing_id, code).await,
        Request::Forget { state_dir, server } => {
            let store = TrustStore::open(&state_dir)?;
            store.lock()?.forget(server.address, &server.server_name)?;
            Ok(json!({}))
        }
        Request::TrustImport {
            state_dir,
            server,
            sha256,
            certificate,
        } => {
            let fingerprint = match (sha256, certificate) {
                (Some(value), None) => parse_sha256_fingerprint(&value)?,
                (None, Some(path)) => certificate_sha256(
                    load_certificates(&path)?
                        .first()
                        .ok_or_else(|| BridgeError::invalid("the certificate bundle is empty"))?,
                ),
                _ => {
                    return Err(BridgeError::invalid(
                        "supply exactly one of a fingerprint or a certificate",
                    ));
                }
            };
            let store = TrustStore::open(&state_dir)?;
            let guard = store.lock()?;
            store.ensure_unpinned(server.address, &server.server_name)?;
            guard.pin(server.address, &server.server_name, fingerprint)?;
            Ok(json!({ "sha256": hex::encode(fingerprint) }))
        }
        Request::Connect {
            state_dir,
            server,
            trust,
            username,
            password,
            timeout_ms,
        } => {
            session::connect(
                &state_dir,
                &server,
                &trust,
                username,
                password,
                timeout_ms.map(Duration::from_millis),
            )
            .await
        }
        Request::Disconnect { session } => Ok(session::disconnect(session)),
        Request::Ping { session: id } => session::ping(&*session::session(id)?).await,
        Request::Capabilities { session: id } => {
            let session = session::session(id)?;
            Ok(
                serde_json::to_value(session.filesystem.capabilities().await?)
                    .map_err(|error| BridgeError::invalid(error.to_string()))?,
            )
        }
        Request::FilesystemStats { session: id } => {
            let session = session::session(id)?;
            Ok(
                serde_json::to_value(session.filesystem.stat_filesystem().await?)
                    .map_err(|error| BridgeError::invalid(error.to_string()))?,
            )
        }
        Request::List { session: id, node } => session::list(&*session::session(id)?, node).await,
        Request::Stat { session: id, node } => session::stat(&*session::session(id)?, node).await,
        Request::ResolvePath { session: id, path } => {
            session::resolve(&*session::session(id)?, &path).await
        }
        Request::ReadLink { session: id, node } => {
            session::read_link(&*session::session(id)?, node).await
        }
        Request::Read {
            session: id,
            node,
            offset,
            length,
        } => session::read(&*session::session(id)?, node, offset, length).await,
        Request::CreateDirectory {
            session: id,
            parent,
            name,
            raw_name,
            mode,
        } => {
            let name = session::parse_name(&name, raw_name.as_deref())?;
            session::create_directory(&*session::session(id)?, parent, name, mode).await
        }
        Request::CreateFile {
            session: id,
            parent,
            name,
            raw_name,
            mode,
        } => {
            let name = session::parse_name(&name, raw_name.as_deref())?;
            session::create_file(&*session::session(id)?, parent, name, mode).await
        }
        Request::Remove {
            session: id,
            parent,
            name,
            raw_name,
            directory,
            recursive,
        } => {
            let name = session::parse_name(&name, raw_name.as_deref())?;
            session::remove(&*session::session(id)?, parent, name, directory, recursive).await
        }
        Request::Rename {
            session: id,
            parent,
            name,
            raw_name,
            new_parent,
            new_name,
            no_replace,
        } => {
            let name = session::parse_name(&name, raw_name.as_deref())?;
            let new_name = session::parse_name(&new_name, None)?;
            session::rename(
                &*session::session(id)?,
                parent,
                name,
                new_parent,
                new_name,
                no_replace,
            )
            .await
        }
        Request::Truncate {
            session: id,
            node,
            size,
        } => session::set_attributes(&*session::session(id)?, node, Some(size), None).await,
        Request::SetMode {
            session: id,
            node,
            mode,
        } => {
            session::set_attributes(&*session::session(id)?, node, None, Some(mode & 0o7777)).await
        }
        Request::Download {
            session: id,
            node,
            destination,
            overwrite,
        } => {
            let session = session::session(id)?;
            transfer::start(runtime, move |progress| {
                transfer::download(session, node_id(node), destination, overwrite, progress)
            })
        }
        Request::Upload {
            session: id,
            parent,
            source,
            name,
            replace,
        } => {
            let session = session::session(id)?;
            let name = match name {
                Some(name) => session::parse_name(&name, None)?,
                None => {
                    use std::os::unix::ffi::OsStrExt;
                    let file_name = source
                        .file_name()
                        .ok_or_else(|| BridgeError::invalid("the upload source has no name"))?;
                    quickfs_protocol::Name::new(file_name.as_bytes().to_vec())
                }
            };
            model::validate_name(name.as_bytes())?;
            transfer::start(runtime, move |progress| {
                transfer::upload(session, node_id(parent), source, name, replace, progress)
            })
        }
        Request::WriteAt {
            session: id,
            node,
            source,
            offset,
            sync,
        } => {
            let session = session::session(id)?;
            transfer::start(runtime, move |progress| {
                transfer::write_at(session, node_id(node), source, offset, sync, progress)
            })
        }
        Request::TransferStatus { transfer: id } => transfer::status(id),
        Request::TransferCancel { transfer: id } => transfer::cancel(id),
        Request::TransferForget { transfer: id } => Ok(transfer::forget(id)),
    }
}

async fn pair(
    state_dir: PathBuf,
    server: Endpoint,
    pairing_id: Uuid,
    code: Zeroizing<String>,
) -> BridgeResult<Value> {
    let store = TrustStore::open(&state_dir)?;
    store.ensure_unpinned(server.address, &server.server_name)?;
    // Recheck under the writer lock before consuming the one-time code.
    let guard = store.lock()?;
    store.ensure_unpinned(server.address, &server.server_name)?;
    let transport =
        PairingClient::connect(server.address, &server.server_name, PAIRING_TIMEOUT).await?;
    let verified = verify_pairing(&transport, pairing_id, &code).await;
    transport.close();
    let fingerprint = verified?;
    guard.pin(server.address, &server.server_name, fingerprint)?;
    Ok(json!({ "sha256": hex::encode(fingerprint) }))
}

/// Resolve `host`, `host:port`, `ip`, `ip:port`, or `[v6]:port` into socket
/// addresses, defaulting the port, and suggest the TLS server name.
async fn resolve(host: &str) -> BridgeResult<Value> {
    let host = host.trim();
    if host.is_empty() {
        return Err(BridgeError::invalid("enter a server address"));
    }
    let (name, port) = split_host_port(host)?;
    let addresses: Vec<String> = tokio::net::lookup_host((name.as_str(), port))
        .await?
        .map(|address| address.to_string())
        .collect();
    if addresses.is_empty() {
        return Err(BridgeError::invalid(format!("'{name}' did not resolve")));
    }
    let server_name = if name.parse::<std::net::IpAddr>().is_ok() {
        "localhost".to_owned()
    } else {
        name.clone()
    };
    Ok(json!({ "addresses": addresses, "host": name, "port": port, "server_name": server_name }))
}

fn split_host_port(value: &str) -> BridgeResult<(String, u16)> {
    let parse_port = |port: &str| {
        port.parse::<u16>()
            .map_err(|_| BridgeError::invalid(format!("'{port}' is not a valid port")))
    };
    if let Some(rest) = value.strip_prefix('[') {
        let (address, tail) = rest
            .split_once(']')
            .ok_or_else(|| BridgeError::invalid("missing ']' in an IPv6 address"))?;
        let port = match tail.strip_prefix(':') {
            Some(port) => parse_port(port)?,
            None if tail.is_empty() => DEFAULT_PORT,
            None => {
                return Err(BridgeError::invalid(
                    "unexpected text after an IPv6 address",
                ));
            }
        };
        return Ok((address.to_owned(), port));
    }
    if value.parse::<std::net::Ipv6Addr>().is_ok() {
        return Ok((value.to_owned(), DEFAULT_PORT));
    }
    match value.rsplit_once(':') {
        Some((host, port)) => Ok((host.to_owned(), parse_port(port)?)),
        None => Ok((value.to_owned(), DEFAULT_PORT)),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn call(request: Value) -> Value {
        serde_json::from_str(&call_json(&request.to_string())).unwrap()
    }

    #[test]
    fn host_parsing_defaults_the_port_and_accepts_ipv6() {
        assert_eq!(split_host_port("nas").unwrap(), ("nas".into(), 4433));
        assert_eq!(
            split_host_port("10.0.0.74:9000").unwrap(),
            ("10.0.0.74".into(), 9000)
        );
        assert_eq!(split_host_port("[::1]:7").unwrap(), ("::1".into(), 7));
        assert_eq!(split_host_port("::1").unwrap(), ("::1".into(), 4433));
        assert!(split_host_port("nas:99999").is_err());
    }

    #[test]
    fn replies_carry_ok_or_a_coded_error() {
        let environment = call(json!({ "op": "environment" }));
        assert!(environment["ok"]["max_client_read_size"].as_u64().unwrap() > 0);

        let malformed = call(json!({ "op": "nope" }));
        assert_eq!(malformed["error"]["code"], "invalid");

        let missing = call(json!({ "op": "ping", "session": 999_999 }));
        assert_eq!(missing["error"]["code"], "session");

        let resolved = call(json!({ "op": "resolve", "host": "127.0.0.1" }));
        assert_eq!(resolved["ok"]["addresses"][0], "127.0.0.1:4433");
        assert_eq!(resolved["ok"]["server_name"], "localhost");
    }

    #[test]
    fn trust_requests_share_the_cli_store() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state");
        let server = json!({ "address": "127.0.0.1:4433", "server_name": "localhost" });
        let empty = call(json!({ "op": "trust_list", "state_dir": state }));
        assert_eq!(empty["ok"]["servers"].as_array().unwrap().len(), 0);

        let imported = call(json!({
            "op": "trust_import", "state_dir": state, "server": server, "sha256": "07".repeat(32)
        }));
        assert_eq!(imported["ok"]["sha256"], "07".repeat(32));
        let again = call(json!({
            "op": "trust_import", "state_dir": state, "server": server, "sha256": "08".repeat(32)
        }));
        assert_eq!(again["error"]["code"], "already_pinned");

        let listed = call(json!({ "op": "trust_list", "state_dir": state }));
        assert_eq!(listed["ok"]["servers"][0]["server_name"], "localhost");

        let forgotten = call(json!({ "op": "forget", "state_dir": state, "server": server }));
        assert!(forgotten.get("ok").is_some());
        let unknown = call(json!({ "op": "forget", "state_dir": state, "server": server }));
        assert_eq!(unknown["error"]["code"], "unpinned");
    }
}
