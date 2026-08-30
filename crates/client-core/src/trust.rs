// SPDX-License-Identifier: Apache-2.0
//! Server-validation policy and the private trust database behind it.
//!
//! The database records, for one address and server name, the exact certificate
//! fingerprint this client has decided to accept. Everything about how it is
//! stored is a security control rather than a convenience:
//!
//! * It lives in a directory only the client account can reach — `0700`/`0600`
//!   on Unix, an owner-only protected DACL on Windows — because anything that
//!   can write it can redirect the mount to a server of its choosing.
//! * Reads validate that protection before parsing, and refuse rather than
//!   fall back, so a widened permission is noticed instead of tolerated.
//! * A pin is never silently replaced. Re-pairing a server that already has a
//!   different identity requires an explicit `forget` first, which is what
//!   turns a man-in-the-middle attempt into a visible error.
//!
//! Both the macOS and Windows clients use exactly this code. Duplicating it per
//! platform is how the two would drift apart on which of those rules still
//! holds.

use quickfs_transport_quic::{QuicClient, TransportError};
use rustls::pki_types::CertificateDer;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    io::Read,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

const MAX_TRUST_DATABASE_SIZE: u64 = 4 * 1024 * 1024;
const MAX_TRUSTED_SERVERS: usize = 4_096;

/// The trust database's file name inside the client state directory.
pub const TRUST_DATABASE_FILE: &str = "trusted-servers.json";

/// The lock file that serialises trust-store mutations between processes.
const TRUST_LOCK_FILE: &str = ".trusted-servers.lock";

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
    #[error("this server already has a pinned identity; use `forget` before deliberate re-pairing")]
    AlreadyPinned,
    #[error(
        "a different identity is already pinned for this server; remove it explicitly before re-pairing"
    )]
    IdentityConflict,
    #[error("another client trust-store operation is in progress")]
    Busy,
    #[error("client trust database has reached its server limit")]
    Full,
}

#[derive(Default, Deserialize, Serialize)]
struct TrustDatabase {
    servers: Vec<TrustedServer>,
}

#[derive(Clone, Deserialize, Serialize)]
struct TrustedServer {
    address: SocketAddr,
    server_name: String,
    certificate_sha256: String,
}

/// One pinned server identity, for listing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PinnedServer {
    pub address: SocketAddr,
    pub server_name: String,
    pub certificate_sha256: [u8; 32],
}

/// Load an exact certificate pin using the same ownership, permission, size,
/// uniqueness, and format checks as the interactive CLI trust path.
pub fn load_trusted_server_pin(
    root: &Path,
    address: SocketAddr,
    server_name: &str,
) -> Result<[u8; 32], TrustStoreError> {
    let path = root.join(TRUST_DATABASE_FILE);
    let database = read_database(root, &path)?;
    let record = database
        .servers
        .iter()
        .find(|record| record.address == address && record.server_name == server_name)
        .ok_or(TrustStoreError::NoPinnedIdentity)?;
    decode_fingerprint(&record.certificate_sha256)
}

/// The private client state directory holding the trust database.
///
/// Opening one creates the directory if it is absent and, either way, brings
/// its permissions to owner-only before anything is read or written.
pub struct TrustStore {
    root: PathBuf,
}

impl TrustStore {
    pub fn open(root: &Path) -> Result<Self, TrustStoreError> {
        create_private_directory(root)?;
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self) -> PathBuf {
        self.root.join(TRUST_DATABASE_FILE)
    }

    /// Every pinned identity, in stored order.
    pub fn list(&self) -> Result<Vec<PinnedServer>, TrustStoreError> {
        let path = self.path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        read_database(&self.root, &path)?
            .servers
            .into_iter()
            .map(|record| {
                Ok(PinnedServer {
                    address: record.address,
                    server_name: record.server_name,
                    certificate_sha256: decode_fingerprint(&record.certificate_sha256)?,
                })
            })
            .collect()
    }

    /// Whether an identity is already pinned for this address and server name.
    pub fn is_pinned(
        &self,
        address: SocketAddr,
        server_name: &str,
    ) -> Result<bool, TrustStoreError> {
        Ok(self
            .list()?
            .into_iter()
            .any(|record| record.address == address && record.server_name == server_name))
    }

    /// Fail unless this address and server name are still unpinned.
    ///
    /// Called before pairing and before importing a managed pin, and called
    /// again *after* any interactive prompt: a prompt can be arbitrarily long,
    /// and another process may have pinned the same server meanwhile.
    pub fn ensure_unpinned(
        &self,
        address: SocketAddr,
        server_name: &str,
    ) -> Result<(), TrustStoreError> {
        if self.is_pinned(address, server_name)? {
            return Err(TrustStoreError::AlreadyPinned);
        }
        Ok(())
    }

    /// Take the cross-process lock that serialises mutations.
    ///
    /// Held for the whole of a pair or import, including the second
    /// `ensure_unpinned` check, so two clients cannot both decide a server is
    /// unpinned and then both write.
    pub fn lock(&self) -> Result<TrustStoreGuard<'_>, TrustStoreError> {
        create_private_directory(&self.root)?;
        let path = self.root.join(TRUST_LOCK_FILE);
        let mut options = fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                TrustStoreError::Busy
            } else {
                TrustStoreError::Io(error)
            }
        })?;
        Ok(TrustStoreGuard {
            store: self,
            path,
            _file: file,
        })
    }
}

/// The exclusive-mutation guard for one trust store.
pub struct TrustStoreGuard<'store> {
    store: &'store TrustStore,
    path: PathBuf,
    _file: fs::File,
}

impl Drop for TrustStoreGuard<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl TrustStoreGuard<'_> {
    pub fn store(&self) -> &TrustStore {
        self.store
    }

    /// Record a pin for this address and server name.
    ///
    /// Pinning the identity that is already recorded succeeds and changes
    /// nothing. Pinning a *different* identity is refused: silently replacing a
    /// pin is exactly what an attacker who reached the pairing exchange would
    /// need, so the operator has to remove the old one deliberately first.
    pub fn pin(
        &self,
        address: SocketAddr,
        server_name: &str,
        fingerprint: [u8; 32],
    ) -> Result<(), TrustStoreError> {
        if server_name.is_empty() || server_name.len() > 253 {
            return Err(TrustStoreError::Invalid(
                "server name must contain between 1 and 253 bytes",
            ));
        }
        let path = self.store.path();
        let mut database = if path.exists() {
            read_database(&self.store.root, &path)?
        } else {
            TrustDatabase::default()
        };
        if let Some(record) = database
            .servers
            .iter()
            .find(|record| record.address == address && record.server_name == server_name)
        {
            return if decode_fingerprint(&record.certificate_sha256)? == fingerprint {
                Ok(())
            } else {
                Err(TrustStoreError::IdentityConflict)
            };
        }
        if database.servers.len() >= MAX_TRUSTED_SERVERS {
            return Err(TrustStoreError::Full);
        }
        database.servers.push(TrustedServer {
            address,
            server_name: server_name.to_owned(),
            certificate_sha256: hex::encode(fingerprint),
        });
        write_database(&path, &database)
    }

    /// Remove a pin so deliberate re-pairing can occur.
    pub fn forget(&self, address: SocketAddr, server_name: &str) -> Result<(), TrustStoreError> {
        let path = self.store.path();
        let mut database = read_database(&self.store.root, &path)?;
        let before = database.servers.len();
        database
            .servers
            .retain(|record| record.address != address || record.server_name != server_name);
        if database.servers.len() == before {
            return Err(TrustStoreError::NoPinnedIdentity);
        }
        write_database(&path, &database)
    }
}

fn decode_fingerprint(value: &str) -> Result<[u8; 32], TrustStoreError> {
    let decoded = hex::decode(value)
        .map_err(|_| TrustStoreError::Invalid("pinned certificate fingerprint is malformed"))?;
    decoded.try_into().map_err(|_| {
        TrustStoreError::Invalid("pinned certificate fingerprint has the wrong length")
    })
}

/// Parse a SHA-256 fingerprint in any of the spellings a certificate tool
/// prints: bare hex, colon separated, or with embedded whitespace.
pub fn parse_sha256_fingerprint(value: &str) -> Result<[u8; 32], TrustStoreError> {
    let normalized: String = value
        .chars()
        .filter(|character| *character != ':' && !character.is_ascii_whitespace())
        .collect();
    if normalized.len() != 64 || !normalized.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(TrustStoreError::Invalid(
            "SHA-256 fingerprint must contain exactly 64 hexadecimal digits",
        ));
    }
    decode_fingerprint(&normalized)
}

fn read_database(root: &Path, path: &Path) -> Result<TrustDatabase, TrustStoreError> {
    validate_trust_store(root, path)?;
    let file = fs::File::open(path)?;
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
    Ok(database)
}

/// Write the database through a temporary file and a rename, so a crash or a
/// full disk leaves either the old database or the new one and never a partial
/// file that would fail every later validation.
fn write_database(path: &Path, database: &TrustDatabase) -> Result<(), TrustStoreError> {
    validate_database(database)?;
    let bytes = serde_json::to_vec_pretty(database)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_TRUST_DATABASE_SIZE {
        return Err(TrustStoreError::Invalid(
            "client trust database exceeds its safety limit",
        ));
    }
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = write_private_file(&temporary, &bytes);
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(TrustStoreError::Io(error));
    }
    Ok(())
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), TrustStoreError> {
    use std::io::Write;

    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    // On Windows the file inherits the directory's DACL, which the directory
    // itself already restricts to this account; tightening the file as well
    // makes it independent of that inheritance.
    harden_private_path(path)?;
    Ok(())
}

/// Create the client state directory, or bring an existing one up to
/// owner-only, before anything is stored in it.
fn create_private_directory(root: &Path) -> Result<(), TrustStoreError> {
    #[cfg(windows)]
    {
        if !root.exists() {
            if let Some(parent) = root.parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(parent)?;
            }
            // Created with its restrictive descriptor already attached, so the
            // directory is never briefly readable by other accounts.
            match quickfs_windows_support::create_private_directory(root) {
                Ok(()) => return Ok(()),
                // A concurrent client won the race; fall through and harden.
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(TrustStoreError::Io(error)),
            }
        }
    }
    fs::create_dir_all(root)?;
    let metadata = fs::symlink_metadata(root)?;
    if !metadata.file_type().is_dir() {
        return Err(TrustStoreError::Invalid(
            "client state path is not a real directory",
        ));
    }
    harden_private_path(root)
}

/// Restrict a path to the client account, on whichever platform this is.
fn harden_private_path(path: &Path) -> Result<(), TrustStoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let metadata = fs::symlink_metadata(path)?;
        validate_owner(path, &metadata)?;
        let mode = if metadata.file_type().is_dir() {
            0o700
        } else {
            0o600
        };
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
        Ok(())
    }
    #[cfg(windows)]
    {
        quickfs_windows_support::harden_private_path(path).map_err(TrustStoreError::Io)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(())
    }
}

/// The trust store's protection check, run before every read.
///
/// A store that is not private is refused outright rather than read anyway:
/// the point of the pin is that nothing else could have chosen it.
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
        use std::os::unix::fs::PermissionsExt;

        validate_owner(root, &root_metadata)?;
        validate_owner(path, &file_metadata)?;
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

    #[cfg(windows)]
    {
        // The Windows equivalent of the owner and mode checks above: the owner
        // must be this account and the DACL must be protected and name nobody
        // else. Anything looser and some other account could have chosen the
        // pin this client is about to trust.
        for candidate in [root, path] {
            if !quickfs_windows_support::validate_private_owner(candidate)
                .map_err(TrustStoreError::Io)?
            {
                return Err(TrustStoreError::Invalid(
                    "private client state must be reachable only by the client account",
                ));
            }
        }
    }

    Ok(())
}

#[cfg(unix)]
fn validate_owner(_path: &Path, metadata: &fs::Metadata) -> Result<(), TrustStoreError> {
    use std::os::unix::fs::MetadataExt;

    if metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(TrustStoreError::Invalid(
            "private client state must be owned by the effective client user",
        ));
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

    fn address() -> SocketAddr {
        #[allow(clippy::expect_used)]
        "127.0.0.1:4433".parse().expect("a literal address")
    }

    #[test]
    fn a_pin_survives_a_round_trip_through_the_store() {
        let directory = tempfile::tempdir().unwrap();
        let store = TrustStore::open(directory.path()).unwrap();
        assert!(store.list().unwrap().is_empty());

        store
            .lock()
            .unwrap()
            .pin(address(), "localhost", [7; 32])
            .unwrap();
        assert_eq!(
            load_trusted_server_pin(directory.path(), address(), "localhost").unwrap(),
            [7; 32]
        );
        assert_eq!(
            store.list().unwrap(),
            vec![PinnedServer {
                address: address(),
                server_name: "localhost".into(),
                certificate_sha256: [7; 32],
            }]
        );
    }

    #[test]
    fn a_different_identity_never_replaces_an_existing_pin() {
        // This is the control that turns an attempted man-in-the-middle into a
        // visible error rather than a silent re-pin.
        let directory = tempfile::tempdir().unwrap();
        let store = TrustStore::open(directory.path()).unwrap();
        store
            .lock()
            .unwrap()
            .pin(address(), "localhost", [7; 32])
            .unwrap();

        assert!(matches!(
            store.lock().unwrap().pin(address(), "localhost", [8; 32]),
            Err(TrustStoreError::IdentityConflict)
        ));
        // Re-pinning the same identity is idempotent, not an error.
        store
            .lock()
            .unwrap()
            .pin(address(), "localhost", [7; 32])
            .unwrap();
        assert_eq!(
            load_trusted_server_pin(directory.path(), address(), "localhost").unwrap(),
            [7; 32]
        );
        assert!(matches!(
            store.ensure_unpinned(address(), "localhost"),
            Err(TrustStoreError::AlreadyPinned)
        ));
    }

    #[test]
    fn forgetting_a_pin_allows_deliberate_repinning_and_nothing_else() {
        let directory = tempfile::tempdir().unwrap();
        let store = TrustStore::open(directory.path()).unwrap();
        store
            .lock()
            .unwrap()
            .pin(address(), "localhost", [7; 32])
            .unwrap();

        assert!(matches!(
            store.lock().unwrap().forget(address(), "other"),
            Err(TrustStoreError::NoPinnedIdentity)
        ));
        store
            .lock()
            .unwrap()
            .forget(address(), "localhost")
            .unwrap();
        assert!(matches!(
            load_trusted_server_pin(directory.path(), address(), "localhost"),
            Err(TrustStoreError::NoPinnedIdentity)
        ));
        store
            .lock()
            .unwrap()
            .pin(address(), "localhost", [8; 32])
            .unwrap();
        assert_eq!(
            load_trusted_server_pin(directory.path(), address(), "localhost").unwrap(),
            [8; 32]
        );
    }

    #[test]
    fn one_mutation_at_a_time_may_hold_the_store() {
        let directory = tempfile::tempdir().unwrap();
        let store = TrustStore::open(directory.path()).unwrap();
        let held = store.lock().unwrap();
        assert!(matches!(store.lock(), Err(TrustStoreError::Busy)));
        drop(held);
        assert!(store.lock().is_ok());
    }

    #[test]
    fn a_pin_is_found_only_under_the_exact_address_and_server_name() {
        let directory = tempfile::tempdir().unwrap();
        let store = TrustStore::open(directory.path()).unwrap();
        store
            .lock()
            .unwrap()
            .pin(address(), "localhost", [7; 32])
            .unwrap();

        assert!(matches!(
            load_trusted_server_pin(directory.path(), address(), "different"),
            Err(TrustStoreError::NoPinnedIdentity)
        ));
        assert!(matches!(
            load_trusted_server_pin(
                directory.path(),
                "127.0.0.1:4434".parse().unwrap(),
                "localhost"
            ),
            Err(TrustStoreError::NoPinnedIdentity)
        ));
    }

    #[test]
    fn fingerprints_are_accepted_in_every_spelling_a_tool_prints() {
        let colons = std::iter::repeat_n("07", 32).collect::<Vec<_>>().join(":");
        assert_eq!(parse_sha256_fingerprint(&colons).unwrap(), [7; 32]);
        assert_eq!(parse_sha256_fingerprint(&"07".repeat(32)).unwrap(), [7; 32]);
        assert_eq!(
            parse_sha256_fingerprint(&format!("  {}  ", "07".repeat(32))).unwrap(),
            [7; 32]
        );
        for rejected in ["07", &"07".repeat(33), &"zz".repeat(32)] {
            assert!(parse_sha256_fingerprint(rejected).is_err(), "{rejected}");
        }
    }

    #[test]
    fn a_store_the_client_account_cannot_own_alone_is_refused() {
        // The check that makes the pin meaningful: if anything else could have
        // written the database, the identity it names proves nothing.
        let directory = tempfile::tempdir().unwrap();
        let store = TrustStore::open(directory.path()).unwrap();
        store
            .lock()
            .unwrap()
            .pin(address(), "localhost", [7; 32])
            .unwrap();
        assert!(load_trusted_server_pin(directory.path(), address(), "localhost").is_ok());

        widen(&directory.path().join(TRUST_DATABASE_FILE));
        assert!(matches!(
            load_trusted_server_pin(directory.path(), address(), "localhost"),
            Err(TrustStoreError::Invalid(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn the_store_is_created_owner_private_and_a_symlinked_root_is_refused() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state");
        let store = TrustStore::open(&state).unwrap();
        store
            .lock()
            .unwrap()
            .pin(address(), "localhost", [7; 32])
            .unwrap();

        assert_eq!(
            fs::metadata(&state).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(state.join(TRUST_DATABASE_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        // A state root that is a symlink is refused outright. Following one
        // would let anything that can create a link in the parent directory
        // choose where the pin is read from and written to.
        let target = directory.path().join("target");
        fs::create_dir(&target).unwrap();
        let linked = directory.path().join("linked");
        symlink(&target, &linked).unwrap();
        assert!(TrustStore::open(&linked).is_err());
        assert!(!target.join(TRUST_DATABASE_FILE).exists());
    }

    /// Make a path readable by accounts other than this one, in whatever way
    /// the platform expresses that.
    #[cfg(unix)]
    fn widen(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[cfg(windows)]
    fn widen(path: &Path) {
        // Replace the protected owner-only DACL with an inherited one, which is
        // what a file copied in by another tool would carry.
        let widened = path.with_extension("widened");
        fs::copy(path, &widened).unwrap();
        fs::remove_file(path).unwrap();
        fs::rename(&widened, path).unwrap();
    }

    #[cfg(not(any(unix, windows)))]
    fn widen(_path: &Path) {}
}
