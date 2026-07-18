// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop};

pub const PROTOCOL_VERSION: u16 = 3;
pub const ALPN_PROTOCOL: &[u8] = b"quickfs/3";
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

/// A wire-compatible UTF-8 string whose debug representation is redacted and
/// whose allocation is cleared when it is dropped.
#[derive(Clone, Default, Eq, PartialEq, Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
#[serde(transparent)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<String> for SecretString {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretString([REDACTED])")
    }
}

/// A fixed-size authentication proof whose debug representation is redacted
/// and whose bytes are cleared when it is dropped.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
#[serde(transparent)]
pub struct SecretProof([u8; 32]);

impl SecretProof {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<[u8; 32]> for SecretProof {
    fn from(value: [u8; 32]) -> Self {
        Self(value)
    }
}

impl std::fmt::Debug for SecretProof {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretProof([REDACTED])")
    }
}

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
    Pair {
        pairing_id: Uuid,
        client_nonce: [u8; 32],
        client_proof: SecretProof,
    },
    Authenticate {
        username: String,
        password: SecretString,
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

impl Request {
    pub fn clear_secrets(&mut self) {
        match self {
            Self::Pair { client_proof, .. } => client_proof.zeroize(),
            Self::Authenticate { password, .. } => password.zeroize(),
            _ => {}
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Response {
    HelloAck {
        version: u16,
    },
    AuthenticateAck,
    PairingProof {
        certificate_fingerprint: [u8; 32],
        proof: SecretProof,
    },
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

    #[test]
    fn authentication_debug_output_redacts_password() {
        let request = Request::Authenticate {
            username: "alice".into(),
            password: "correct horse battery staple".to_string().into(),
        };
        let debug = format!("{request:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("correct horse"));
    }

    #[test]
    fn pairing_debug_output_redacts_proofs() {
        let request = Request::Pair {
            pairing_id: Uuid::nil(),
            client_nonce: [1; 32],
            client_proof: [2; 32].into(),
        };
        let output = format!("{request:?}");
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("2, 2, 2"));
    }
}
