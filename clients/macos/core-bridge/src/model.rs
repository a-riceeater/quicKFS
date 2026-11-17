// SPDX-License-Identifier: Apache-2.0
//! Serializable shapes shared with the Swift app, plus the bridge error type.

use quickfs_client_core::{ClientError, TrustStoreError};
use quickfs_protocol::{Metadata, Name, NodeId, NodeKind};
use quickfs_transport_quic::TransportError;
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, path::PathBuf};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("{0}")]
    Invalid(String),
    #[error("{}", client_message(.0))]
    Client(#[from] ClientError),
    #[error("{0}")]
    Trust(#[from] TrustStoreError),
    #[error("{0}")]
    Transport(#[from] TransportError),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("this connection has closed; connect again")]
    UnknownSession,
    #[error("this transfer is no longer tracked")]
    UnknownTransfer,
    #[error("cancelled")]
    Cancelled,
}

pub type BridgeResult<T> = Result<T, BridgeError>;

/// The server's own message without the `server: Code:` prefix, since the
/// code travels separately in the reply.
fn client_message(error: &ClientError) -> String {
    match error {
        ClientError::Server(_, message) if !message.is_empty() => message.clone(),
        other => other.to_string(),
    }
}

impl BridgeError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }

    /// A stable machine-readable code so the app can react to specific
    /// failures (for example asking for a password again) without parsing
    /// human-readable text.
    pub fn code(&self) -> String {
        match self {
            Self::Invalid(_) => "invalid".into(),
            Self::Client(ClientError::Server(code, _)) => format!("{code:?}"),
            Self::Client(ClientError::Transport(_)) | Self::Transport(_) => "transport".into(),
            Self::Client(ClientError::Offline) => "offline".into(),
            Self::Client(ClientError::StaleRevision) => "stale".into(),
            Self::Client(ClientError::AmbiguousMutation) => "ambiguous".into(),
            Self::Client(_) => "client".into(),
            Self::Trust(TrustStoreError::NoPinnedIdentity) => "unpinned".into(),
            Self::Trust(TrustStoreError::AlreadyPinned) => "already_pinned".into(),
            Self::Trust(TrustStoreError::DifferentIdentityPinned) => "identity_mismatch".into(),
            Self::Trust(TrustStoreError::Busy) => "busy".into(),
            Self::Trust(_) => "trust".into(),
            Self::Io(_) => "io".into(),
            Self::UnknownSession => "session".into(),
            Self::UnknownTransfer => "transfer".into(),
            Self::Cancelled => "cancelled".into(),
        }
    }
}

/// A server address plus the TLS name its certificate must present.
#[derive(Clone, Debug, Deserialize)]
pub struct Endpoint {
    pub address: SocketAddr,
    pub server_name: String,
}

/// How the app validates a server before it sends any credential.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TrustSpec {
    /// The exact certificate pin recorded by pairing or managed import.
    Pinned,
    /// A PEM enterprise CA bundle plus the server name.
    EnterpriseCa { certificate: PathBuf },
    /// The operating system's public and managed roots plus the server name.
    SystemRoots,
}

/// A directory entry or stat result in a Swift-friendly shape.
#[derive(Debug, Serialize)]
pub struct NodeInfo {
    pub node: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Hex of the exact name bytes, present only when the name is not valid
    /// UTF-8 and `name` is therefore a lossy display form.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_name: Option<String>,
    pub kind: &'static str,
    pub size: u64,
    pub mode: u32,
    pub allocated_bytes: u64,
    pub revision: u64,
    pub accessed_ms: u64,
    pub modified_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_ms: Option<u64>,
    pub link_count: u32,
}

impl NodeInfo {
    pub fn from_metadata(metadata: &Metadata, name: Option<&Name>) -> Self {
        let (name, raw_name) = match name {
            None => (None, None),
            Some(name) => match std::str::from_utf8(name.as_bytes()) {
                Ok(text) => (Some(text.to_owned()), None),
                Err(_) => (
                    Some(String::from_utf8_lossy(name.as_bytes()).into_owned()),
                    Some(hex::encode(name.as_bytes())),
                ),
            },
        };
        Self {
            node: metadata.node.0,
            name,
            raw_name,
            kind: kind_name(metadata.kind),
            size: metadata.size,
            mode: metadata.mode,
            allocated_bytes: metadata.allocated_blocks.saturating_mul(512),
            revision: metadata.revision,
            accessed_ms: metadata.accessed_unix_ms,
            modified_ms: metadata.modified_unix_ms,
            created_ms: metadata.created_unix_ms,
            backup_ms: metadata.backup_unix_ms,
            link_count: metadata.link_count,
        }
    }
}

pub fn kind_name(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::File => "file",
        NodeKind::Directory => "directory",
        NodeKind::Symlink => "symlink",
        NodeKind::NamedPipe => "fifo",
        NodeKind::CharacterDevice => "char_device",
        NodeKind::BlockDevice => "block_device",
        NodeKind::Socket => "socket",
    }
}

/// Rebuild exact name bytes from the app. `raw_name` wins when present so a
/// non-UTF-8 name shown lossily can still be addressed precisely.
pub fn entry_name(name: &str, raw_name: Option<&str>) -> BridgeResult<Name> {
    let bytes = match raw_name {
        Some(raw) => hex::decode(raw).map_err(|_| BridgeError::invalid("malformed raw name"))?,
        None => name.as_bytes().to_vec(),
    };
    validate_name(&bytes)?;
    Ok(Name::new(bytes))
}

pub fn validate_name(bytes: &[u8]) -> BridgeResult<()> {
    if bytes.is_empty() || bytes == b"." || bytes == b".." {
        return Err(BridgeError::invalid("a name cannot be empty, '.', or '..'"));
    }
    if bytes.len() > 255 {
        return Err(BridgeError::invalid("a name cannot exceed 255 bytes"));
    }
    if bytes.contains(&b'/') || bytes.contains(&0) {
        return Err(BridgeError::invalid("a name cannot contain '/' or NUL"));
    }
    Ok(())
}

pub fn node_id(value: Uuid) -> NodeId {
    NodeId(value)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_raw_bytes_and_reject_path_components() {
        let raw = Name::new(vec![b'a', 0xff, b'b']);
        let metadata = Metadata {
            node: NodeId(Uuid::nil()),
            kind: NodeKind::File,
            size: 3,
            mode: 0o644,
            allocated_blocks: 1,
            revision: 9,
            accessed_unix_ms: 0,
            modified_unix_ms: 0,
            created_unix_ms: None,
            backup_unix_ms: None,
            link_count: 1,
            device_major: 0,
            device_minor: 0,
        };
        let info = NodeInfo::from_metadata(&metadata, Some(&raw));
        assert_eq!(info.raw_name.as_deref(), Some("61ff62"));
        assert_eq!(info.allocated_bytes, 512);
        let rebuilt = entry_name(info.name.as_deref().unwrap(), info.raw_name.as_deref()).unwrap();
        assert_eq!(rebuilt, raw);
        assert!(entry_name("a/b", None).is_err());
        assert!(entry_name("..", None).is_err());
        assert!(entry_name("", None).is_err());
    }
}
