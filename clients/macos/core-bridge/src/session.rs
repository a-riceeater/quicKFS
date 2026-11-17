// SPDX-License-Identifier: Apache-2.0
//! Authenticated browsing sessions and the filesystem operations the app
//! performs on them.

use crate::model::{BridgeError, BridgeResult, Endpoint, NodeInfo, TrustSpec, entry_name, node_id};
use quickfs_client_core::{
    AuthenticatedConnectionConfig, ReconnectPolicy, RemoteFilesystem, ResilientFilesystem,
    ServerTrust, load_trusted_server_pin, resolve_path,
};
use quickfs_protocol::{
    AttributeChanges, FileAccess, FileOpenOptions, FilesystemCapabilities, Name, NodeId, NodeKind,
    RenameMode,
};
use quickfs_transport_quic::load_certificates;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use uuid::Uuid;
use zeroize::Zeroizing;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound for one inline preview read so a JSON reply stays small.
pub const MAX_PREVIEW_BYTES: u64 = 1024 * 1024;

pub struct Session {
    pub filesystem: Arc<dyn RemoteFilesystem>,
    pub capabilities: FilesystemCapabilities,
}

static SESSIONS: Mutex<Option<HashMap<u64, Arc<Session>>>> = Mutex::new(None);
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

fn with_sessions<T>(body: impl FnOnce(&mut HashMap<u64, Arc<Session>>) -> T) -> T {
    let mut guard = SESSIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    body(guard.get_or_insert_with(HashMap::new))
}

pub fn session(id: u64) -> BridgeResult<Arc<Session>> {
    with_sessions(|sessions| sessions.get(&id).cloned()).ok_or(BridgeError::UnknownSession)
}

pub fn server_trust(
    state_dir: &Path,
    endpoint: &Endpoint,
    spec: &TrustSpec,
) -> BridgeResult<ServerTrust> {
    Ok(match spec {
        TrustSpec::Pinned => ServerTrust::pinned(load_trusted_server_pin(
            state_dir,
            endpoint.address,
            &endpoint.server_name,
        )?),
        TrustSpec::EnterpriseCa { certificate } => {
            ServerTrust::enterprise_ca(load_certificates(certificate)?)
        }
        TrustSpec::SystemRoots => ServerTrust::system_roots(),
    })
}

pub async fn connect(
    state_dir: &Path,
    endpoint: &Endpoint,
    trust: &TrustSpec,
    username: String,
    password: Zeroizing<String>,
    timeout: Option<Duration>,
) -> BridgeResult<Value> {
    if username.is_empty() {
        return Err(BridgeError::invalid("a username is required"));
    }
    let trust = server_trust(state_dir, endpoint, trust)?;
    let config = AuthenticatedConnectionConfig::new(
        endpoint.address,
        endpoint.server_name.clone(),
        trust,
        username,
        password.to_string(),
        timeout.unwrap_or(DEFAULT_TIMEOUT),
    );
    let resilient = ResilientFilesystem::connect_authenticated(
        config,
        ReconnectPolicy {
            attempts: 3,
            initial_backoff: Duration::from_millis(100),
            maximum_backoff: Duration::from_secs(2),
        },
    )
    .await?;
    let filesystem: Arc<dyn RemoteFilesystem> = Arc::new(resilient);
    let capabilities = filesystem.capabilities().await?;
    let id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    let reply = json!({ "session": id, "capabilities": capabilities });
    with_sessions(|sessions| {
        sessions.insert(
            id,
            Arc::new(Session {
                filesystem,
                capabilities,
            }),
        )
    });
    Ok(reply)
}

pub fn disconnect(id: u64) -> Value {
    let removed = with_sessions(|sessions| sessions.remove(&id)).is_some();
    json!({ "closed": removed })
}

pub async fn ping(session: &Session) -> BridgeResult<Value> {
    let started = Instant::now();
    let nonce = Uuid::new_v4().as_u64_pair().0;
    let echoed = session.filesystem.ping(nonce).await?;
    if echoed != nonce {
        return Err(BridgeError::invalid(
            "the server echoed a different ping nonce",
        ));
    }
    Ok(json!({ "rtt_ms": started.elapsed().as_secs_f64() * 1000.0 }))
}

pub async fn list(session: &Session, node: Uuid) -> BridgeResult<Value> {
    let node = node_id(node);
    let (entries, directory) = tokio::try_join!(
        session.filesystem.list_directory(node),
        session.filesystem.get_metadata(node)
    )?;
    let entries: Vec<NodeInfo> = entries
        .iter()
        .map(|entry| NodeInfo::from_metadata(&entry.metadata, Some(&entry.name)))
        .collect();
    Ok(json!({
        "directory": NodeInfo::from_metadata(&directory, None),
        "entries": entries,
    }))
}

pub async fn stat(session: &Session, node: Uuid) -> BridgeResult<Value> {
    let metadata = session.filesystem.get_metadata(node_id(node)).await?;
    serde_json::to_value(NodeInfo::from_metadata(&metadata, None))
        .map_err(|error| BridgeError::invalid(error.to_string()))
}

pub async fn resolve(session: &Session, path: &str) -> BridgeResult<Value> {
    let node = resolve_path(session.filesystem.as_ref(), path).await?;
    let metadata = session.filesystem.get_metadata(node).await?;
    Ok(json!({ "node": NodeInfo::from_metadata(&metadata, None) }))
}

pub async fn read_link(session: &Session, node: Uuid) -> BridgeResult<Value> {
    let target = session.filesystem.read_link(node_id(node)).await?;
    Ok(json!({ "target": String::from_utf8_lossy(&target) }))
}

/// Read one bounded range, mirroring the CLI `read` command, for previews.
pub async fn read(session: &Session, node: Uuid, offset: u64, length: u64) -> BridgeResult<Value> {
    let length = length.min(MAX_PREVIEW_BYTES);
    let (handle, revision, size) = session.filesystem.open_file(node_id(node)).await?;
    let result = session.filesystem.read_range(handle, offset, length).await;
    let closed = session.filesystem.close_file(handle).await;
    let data = result?;
    closed?;
    Ok(json!({
        "data": hex::encode(&data),
        "size": size,
        "revision": revision,
    }))
}

pub async fn create_directory(
    session: &Session,
    parent: Uuid,
    name: Name,
    mode: u32,
) -> BridgeResult<Value> {
    let parent = node_id(parent);
    let metadata = session
        .filesystem
        .create_directory(parent, name.clone(), mode)
        .await?;
    let _ = session.filesystem.sync_directory(parent).await;
    Ok(json!({ "node": NodeInfo::from_metadata(&metadata, Some(&name)) }))
}

/// Atomically create an empty file, matching the CLI `create` command.
pub async fn create_file(
    session: &Session,
    parent: Uuid,
    name: Name,
    mode: u32,
) -> BridgeResult<Value> {
    let parent = node_id(parent);
    let created = session
        .filesystem
        .create_file(parent, name.clone(), mode, READ_WRITE)
        .await?;
    let handle = created.opened.handle;
    let synced = session.filesystem.sync_file(handle, false).await;
    let closed = session.filesystem.close_file(handle).await;
    synced?;
    closed?;
    session.filesystem.sync_directory(parent).await?;
    Ok(json!({ "node": NodeInfo::from_metadata(&created.metadata, Some(&name)) }))
}

pub const READ_WRITE: FileOpenOptions = FileOpenOptions {
    access: FileAccess::ReadWrite,
    truncate: false,
    append: false,
};

pub async fn remove(
    session: &Session,
    parent: Uuid,
    name: Name,
    directory: bool,
    recursive: bool,
) -> BridgeResult<Value> {
    let parent = node_id(parent);
    let removed = if directory && recursive {
        remove_tree(session.filesystem.as_ref(), parent, name).await?
    } else {
        session
            .filesystem
            .remove_node(parent, name, directory)
            .await?;
        1
    };
    let _ = session.filesystem.sync_directory(parent).await;
    Ok(json!({ "removed": removed }))
}

/// Depth-first removal of a directory and everything below it. The walk is
/// iterative so a deep tree cannot overflow the async stack.
async fn remove_tree(
    filesystem: &dyn RemoteFilesystem,
    parent: NodeId,
    name: Name,
) -> BridgeResult<u64> {
    let root = lookup(filesystem, parent, &name).await?;
    // Each frame is a directory whose children are still being removed.
    let mut stack: Vec<(NodeId, NodeId, Name)> = vec![(parent, root, name)];
    let mut removed = 0_u64;
    while let Some((frame_parent, directory, frame_name)) = stack.last().cloned() {
        let mut descended = false;
        for entry in filesystem.list_directory(directory).await? {
            if entry.kind == NodeKind::Directory {
                stack.push((directory, entry.node, entry.name));
                descended = true;
                break;
            }
            filesystem.remove_node(directory, entry.name, false).await?;
            removed += 1;
        }
        if !descended {
            filesystem
                .remove_node(frame_parent, frame_name, true)
                .await?;
            removed += 1;
            stack.pop();
        }
    }
    Ok(removed)
}

pub async fn lookup(
    filesystem: &dyn RemoteFilesystem,
    parent: NodeId,
    name: &Name,
) -> BridgeResult<NodeId> {
    filesystem
        .list_directory(parent)
        .await?
        .into_iter()
        .find(|entry| &entry.name == name)
        .map(|entry| entry.node)
        .ok_or_else(|| {
            BridgeError::Client(quickfs_client_core::ClientError::Server(
                quickfs_protocol::ErrorCode::NotFound,
                String::from_utf8_lossy(name.as_bytes()).into_owned(),
            ))
        })
}

#[allow(clippy::too_many_arguments)]
pub async fn rename(
    session: &Session,
    parent: Uuid,
    name: Name,
    new_parent: Uuid,
    new_name: Name,
    no_replace: bool,
) -> BridgeResult<Value> {
    let (parent, new_parent) = (node_id(parent), node_id(new_parent));
    session
        .filesystem
        .rename_node(
            parent,
            name,
            new_parent,
            new_name,
            if no_replace {
                RenameMode::NoReplace
            } else {
                RenameMode::Replace
            },
        )
        .await?;
    session.filesystem.sync_directory(parent).await?;
    if new_parent != parent {
        session.filesystem.sync_directory(new_parent).await?;
    }
    Ok(json!({}))
}

pub async fn set_attributes(
    session: &Session,
    node: Uuid,
    size: Option<u64>,
    mode: Option<u32>,
) -> BridgeResult<Value> {
    let metadata = session
        .filesystem
        .set_attributes(
            node_id(node),
            None,
            AttributeChanges {
                size,
                mode,
                accessed_unix_ms: None,
                modified_unix_ms: None,
                backup_unix_ms: None,
            },
        )
        .await?;
    Ok(json!({ "node": NodeInfo::from_metadata(&metadata, None) }))
}

pub fn parse_name(name: &str, raw_name: Option<&str>) -> BridgeResult<Name> {
    entry_name(name, raw_name)
}
