// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;
pub const ROOT_NODE: NodeId = NodeId(Uuid::from_u128(0));

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct RequestId(pub Uuid);
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct NodeId(pub Uuid);
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct FileHandle(pub Uuid);
pub type FileRevision = u64;
pub type DirectoryRevision = u64;

impl RequestId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}
impl Default for RequestId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub version: u16,
    pub request_id: RequestId,
    pub message: T,
}
impl<T> Envelope<T> {
    pub fn new(message: T) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id: RequestId::new(),
            message,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Request {
    Hello {
        client_name: String,
    },
    Authenticate {
        token: String,
    },
    GetMetadata {
        node: NodeId,
    },
    ListDirectory {
        node: NodeId,
    },
    OpenFile {
        node: NodeId,
    },
    ReadRange {
        handle: FileHandle,
        offset: u64,
        length: u64,
    },
    CloseFile {
        handle: FileHandle,
    },
    Ping {
        nonce: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Response {
    HelloAck {
        version: u16,
    },
    AuthenticateAck,
    Metadata(Metadata),
    DirectoryListing {
        revision: DirectoryRevision,
        entries: Vec<DirectoryEntry>,
    },
    FileOpened {
        handle: FileHandle,
        revision: FileRevision,
        size: u64,
    },
    ReadData {
        revision: FileRevision,
        length: u64,
    },
    FileClosed,
    Pong {
        nonce: u64,
    },
    Error(ProtocolError),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Metadata {
    pub node: NodeId,
    pub kind: NodeKind,
    pub size: u64,
    pub revision: u64,
    pub modified_unix_ms: u64,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DirectoryEntry {
    pub node: NodeId,
    pub name: String,
    pub kind: NodeKind,
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum NodeKind {
    File,
    Directory,
    Symlink,
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum ErrorCode {
    Unauthenticated,
    NotFound,
    PermissionDenied,
    InvalidNode,
    InvalidHandle,
    InvalidRequest,
    UnsupportedVersion,
    TooLarge,
    Timeout,
    Internal,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProtocolError {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("frame too large: {0} bytes")]
    TooLarge(usize),
    #[error("malformed message: {0}")]
    Malformed(#[from] postcard::Error),
    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u16),
}

pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, CodecError> {
    let out = postcard::to_allocvec(value)?;
    if out.len() > MAX_FRAME_SIZE {
        return Err(CodecError::TooLarge(out.len()));
    }
    Ok(out)
}
pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, CodecError> {
    if bytes.len() > MAX_FRAME_SIZE {
        return Err(CodecError::TooLarge(bytes.len()));
    }
    Ok(postcard::from_bytes(bytes)?)
}
pub fn decode_request(bytes: &[u8]) -> Result<Envelope<Request>, CodecError> {
    let msg: Envelope<Request> = decode(bytes)?;
    if msg.version != PROTOCOL_VERSION {
        return Err(CodecError::UnsupportedVersion(msg.version));
    }
    Ok(msg)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn round_trip() {
        let m = Envelope::new(Request::Ping { nonce: 42 });
        let b = encode(&m).unwrap();
        assert_eq!(decode_request(&b).unwrap(), m);
    }
    #[test]
    fn rejects_version() {
        let mut m = Envelope::new(Request::Ping { nonce: 1 });
        m.version = 99;
        assert!(matches!(
            decode_request(&encode(&m).unwrap()),
            Err(CodecError::UnsupportedVersion(99))
        ));
    }
    #[test]
    fn rejects_bad_data() {
        assert!(decode_request(&[255, 1]).is_err());
    }
    #[test]
    fn rejects_oversize() {
        assert!(matches!(
            decode::<Request>(&vec![0; MAX_FRAME_SIZE + 1]),
            Err(CodecError::TooLarge(_))
        ));
    }
}
