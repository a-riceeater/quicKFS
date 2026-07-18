// SPDX-License-Identifier: Apache-2.0

use quickfs_transport_quic::{QuicClient, TransportError};
use rustls::pki_types::CertificateDer;
use serde::Deserialize;
use std::{collections::HashSet, fs, io::Read, net::SocketAddr, path::Path, time::Duration};

const MAX_TRUST_DATABASE_SIZE: u64 = 4 * 1024 * 1024;
const MAX_TRUSTED_SERVERS: usize = 4_096;

/// A server-validation policy shared by interactive clients and native mounts.
#[derive(Clone)]
pub enum ServerTrust {
    Pinned([u8; 32]),
    EnterpriseCa(Vec<CertificateDer<'static>>),
    SystemRoots,
}

impl ServerTrust {
    pub fn pinned(fingerprint: [u8; 32]) -> Self {
        Self::Pinned(fingerprint)
    }

    pub fn enterprise_ca(authorities: Vec<CertificateDer<'static>>) -> Self {
        Self::EnterpriseCa(authorities)
    }

    pub fn system_roots() -> Self {
        Self::SystemRoots
    }

    pub async fn connect(
        &self,
        server: SocketAddr,
        server_name: &str,
        timeout: Duration,
    ) -> Result<QuicClient, TransportError> {
        match self {
            Self::Pinned(fingerprint) => {
                QuicClient::connect_pinned(server, server_name, *fingerprint, timeout).await
            }
            Self::EnterpriseCa(authorities) => {
                QuicClient::connect_with_ca(server, server_name, authorities.clone(), timeout).await
            }
            Self::SystemRoots => {
                QuicClient::connect_with_system_roots(server, server_name, timeout).await
            }
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            Self::Pinned(_) => "the configured exact certificate pin",
            Self::EnterpriseCa(_) => "the configured enterprise CA bundle and server name",
            Self::SystemRoots => "the operating-system trust policy and server name",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TrustStoreError {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("client trust database is malformed: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error("{0}")]
    Invalid(&'static str),
    #[error("no pinned identity for this address and server name")]
    NoPinnedIdentity,
}

#[derive(Deserialize)]
struct TrustDatabase {
    servers: Vec<TrustedServer>,
}

#[derive(Deserialize)]
struct TrustedServer {
    address: SocketAddr,
    server_name: String,
    certificate_sha256: String,
}

/// Load an exact certificate pin using the same ownership, permission, size,
/// uniqueness, and format checks as the interactive CLI trust path.
pub fn load_trusted_server_pin(
    root: &Path,
    address: SocketAddr,
    server_name: &str,
) -> Result<[u8; 32], TrustStoreError> {
    let path = root.join("trusted-servers.json");
    validate_trust_store(root, &path)?;

    let file = fs::File::open(&path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_TRUST_DATABASE_SIZE {
        return Err(TrustStoreError::Invalid(
            "client trust database is not regular or exceeds its safety limit",
        ));
    }

    let capacity = usize::try_from(metadata.len()).unwrap_or(0);
    let mut bytes = Vec::with_capacity(capacity);
    file.take(MAX_TRUST_DATABASE_SIZE.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_TRUST_DATABASE_SIZE {
        return Err(TrustStoreError::Invalid(
            "client trust database exceeds its safety limit",
        ));
    }

    let database: TrustDatabase = serde_json::from_slice(&bytes)?;
    validate_database(&database)?;
    let record = database
        .servers
        .iter()
        .find(|record| record.address == address && record.server_name == server_name)
        .ok_or(TrustStoreError::NoPinnedIdentity)?;
    let decoded = hex::decode(&record.certificate_sha256)
        .map_err(|_| TrustStoreError::Invalid("pinned certificate fingerprint is malformed"))?;
    decoded.try_into().map_err(|_| {
        TrustStoreError::Invalid("pinned certificate fingerprint has the wrong length")
    })
}

fn validate_trust_store(root: &Path, path: &Path) -> Result<(), TrustStoreError> {
    let root_metadata = fs::symlink_metadata(root)?;
    if !root_metadata.file_type().is_dir() {
        return Err(TrustStoreError::Invalid(
            "client state path is not a real directory",
        ));
    }
    let file_metadata = fs::symlink_metadata(path)?;
    if !file_metadata.file_type().is_file() {
        return Err(TrustStoreError::Invalid(
            "client trust database is not a regular file",
        ));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let effective_uid = rustix::process::geteuid().as_raw();
        if root_metadata.uid() != effective_uid || file_metadata.uid() != effective_uid {
            return Err(TrustStoreError::Invalid(
                "private client state must be owned by the effective client user",
            ));
        }
        if root_metadata.permissions().mode() & 0o077 != 0 {
            return Err(TrustStoreError::Invalid(
                "client state directory must not be accessible by group or other users",
            ));
        }
        if file_metadata.permissions().mode() & 0o077 != 0 {
            return Err(TrustStoreError::Invalid(
                "client trust database must not be accessible by group or other users",
            ));
        }
    }
    Ok(())
}

fn validate_database(database: &TrustDatabase) -> Result<(), TrustStoreError> {
    if database.servers.len() > MAX_TRUSTED_SERVERS {
        return Err(TrustStoreError::Invalid(
            "client trust database contains too many servers",
        ));
    }

    let mut identities = HashSet::with_capacity(database.servers.len());
    for record in &database.servers {
        if record.server_name.is_empty() || record.server_name.len() > 253 {
            return Err(TrustStoreError::Invalid(
                "client trust database contains an invalid server name",
            ));
        }
        if record.certificate_sha256.len() != 64
            || !record
                .certificate_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(TrustStoreError::Invalid(
                "client trust database contains an invalid certificate fingerprint",
            ));
        }
        if !identities.insert((record.address, record.server_name.clone())) {
            return Err(TrustStoreError::Invalid(
                "client trust database contains a duplicate server identity",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::io::Write;

    #[cfg(unix)]
    #[test]
    fn loads_only_the_selected_private_pin() {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("state");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.join("trusted-servers.json");
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.write_all(
            br#"{"servers":[{"address":"127.0.0.1:4433","server_name":"localhost","certificate_sha256":"0707070707070707070707070707070707070707070707070707070707070707"}]}"#,
        )
        .unwrap();
        drop(file);

        let address = "127.0.0.1:4433".parse().unwrap();
        assert_eq!(
            load_trusted_server_pin(&root, address, "localhost").unwrap(),
            [7; 32]
        );
        assert!(matches!(
            load_trusted_server_pin(&root, address, "different"),
            Err(TrustStoreError::NoPinnedIdentity)
        ));
    }
}
