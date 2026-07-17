// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]

use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop};

const USERS_FILE: &str = "users.json";
const PAIRINGS_DIR: &str = "pairings";
const CERT_FILE: &str = "server.crt";
const KEY_FILE: &str = "server.key";

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid authentication data: {0}")]
    InvalidData(String),
    #[error("user '{0}' already exists")]
    UserExists(String),
    #[error("user '{0}' was not found")]
    UserNotFound(String),
    #[error("pairing session was not found or has already been used")]
    PairingNotFound,
    #[error("pairing session has expired")]
    PairingExpired,
    #[error("cryptographic operation failed")]
    Crypto,
}

#[derive(Clone, Debug)]
pub struct StatePaths {
    pub root: PathBuf,
    pub certificate: PathBuf,
    pub private_key: PathBuf,
}

impl StatePaths {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            certificate: root.join(CERT_FILE),
            private_key: root.join(KEY_FILE),
            root,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct UserRecord {
    username: String,
    password_hash: String,
    enabled: bool,
}

#[derive(Default, Deserialize, Serialize)]
struct UserDatabase {
    users: Vec<UserRecord>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PairingRecord {
    id: Uuid,
    secret: String,
    expires_unix_seconds: u64,
}

#[derive(Zeroize, ZeroizeOnDrop)]
pub struct PairingSecret(Vec<u8>);

impl PairingSecret {
    pub fn proof(
        &self,
        certificate_fingerprint: &[u8; 32],
        client_nonce: &[u8; 32],
    ) -> Result<[u8; 32], AuthError> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.0).map_err(|_| AuthError::Crypto)?;
        mac.update(b"quickfs pairing proof v1\0");
        mac.update(certificate_fingerprint);
        mac.update(client_nonce);
        Ok(mac.finalize().into_bytes().into())
    }

    pub fn verify_proof(
        &self,
        certificate_fingerprint: &[u8; 32],
        client_nonce: &[u8; 32],
        proof: &[u8],
    ) -> bool {
        let mut mac = match Hmac::<Sha256>::new_from_slice(&self.0) {
            Ok(mac) => mac,
            Err(_) => return false,
        };
        mac.update(b"quickfs pairing proof v1\0");
        mac.update(certificate_fingerprint);
        mac.update(client_nonce);
        mac.verify_slice(proof).is_ok()
    }
}

#[derive(Clone, Debug)]
pub struct NewPairing {
    pub id: Uuid,
    pub code: String,
    pub expires_unix_seconds: u64,
}

pub fn initialize(root: &Path, server_names: Vec<String>) -> Result<StatePaths, AuthError> {
    fs::create_dir_all(root)?;
    fs::create_dir_all(root.join(PAIRINGS_DIR))?;
    let paths = StatePaths::new(root);
    if paths.certificate.exists() || paths.private_key.exists() {
        return Err(AuthError::InvalidData(format!(
            "identity files already exist in '{}'",
            root.display()
        )));
    }
    if server_names.is_empty() {
        return Err(AuthError::InvalidData(
            "at least one server name is required".into(),
        ));
    }
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(server_names)
            .map_err(|error| AuthError::InvalidData(error.to_string()))?;
    write_private(&paths.private_key, signing_key.serialize_pem().as_bytes())?;
    fs::write(&paths.certificate, cert.pem())?;
    write_json(&root.join(USERS_FILE), &UserDatabase::default())?;
    Ok(paths)
}

pub fn add_user(root: &Path, username: &str, password: &[u8]) -> Result<(), AuthError> {
    validate_username(username)?;
    if password.len() < 12 {
        return Err(AuthError::InvalidData(
            "password must contain at least 12 bytes".into(),
        ));
    }
    let path = root.join(USERS_FILE);
    let mut database: UserDatabase = read_json(&path)?;
    if database.users.iter().any(|user| user.username == username) {
        return Err(AuthError::UserExists(username.into()));
    }
    database.users.push(UserRecord {
        username: username.into(),
        password_hash: hash_password(password)?,
        enabled: true,
    });
    write_json(&path, &database)
}

pub fn change_password(root: &Path, username: &str, password: &[u8]) -> Result<(), AuthError> {
    if password.len() < 12 {
        return Err(AuthError::InvalidData(
            "password must contain at least 12 bytes".into(),
        ));
    }
    let path = root.join(USERS_FILE);
    let mut database: UserDatabase = read_json(&path)?;
    let user = database
        .users
        .iter_mut()
        .find(|user| user.username == username)
        .ok_or_else(|| AuthError::UserNotFound(username.into()))?;
    user.password_hash = hash_password(password)?;
    write_json(&path, &database)
}

pub fn set_user_enabled(root: &Path, username: &str, enabled: bool) -> Result<(), AuthError> {
    let path = root.join(USERS_FILE);
    let mut database: UserDatabase = read_json(&path)?;
    let user = database
        .users
        .iter_mut()
        .find(|user| user.username == username)
        .ok_or_else(|| AuthError::UserNotFound(username.into()))?;
    user.enabled = enabled;
    write_json(&path, &database)
}

pub fn remove_user(root: &Path, username: &str) -> Result<(), AuthError> {
    let path = root.join(USERS_FILE);
    let mut database: UserDatabase = read_json(&path)?;
    let previous = database.users.len();
    database.users.retain(|user| user.username != username);
    if database.users.len() == previous {
        return Err(AuthError::UserNotFound(username.into()));
    }
    write_json(&path, &database)
}

pub fn verify_user(root: &Path, username: &str, password: &[u8]) -> Result<bool, AuthError> {
    let database: UserDatabase = read_json(&root.join(USERS_FILE))?;
    let Some(user) = database
        .users
        .iter()
        .find(|user| user.username == username && user.enabled)
    else {
        // Equalize the expensive path enough to avoid a trivial username oracle.
        let salt =
            SaltString::encode_b64(b"quickfs-unknown-user-salt").map_err(|_| AuthError::Crypto)?;
        let _ = Argon2::default().hash_password(password, &salt);
        return Ok(false);
    };
    let parsed = PasswordHash::new(&user.password_hash).map_err(|_| {
        AuthError::InvalidData(format!("invalid password hash for user '{username}'"))
    })?;
    Ok(Argon2::default().verify_password(password, &parsed).is_ok())
}

pub fn create_pairing(root: &Path, lifetime: Duration) -> Result<NewPairing, AuthError> {
    if lifetime.is_zero() {
        return Err(AuthError::InvalidData(
            "pairing lifetime must be greater than zero".into(),
        ));
    }
    let id = Uuid::new_v4();
    let mut secret = [0u8; 20];
    getrandom::fill(&mut secret).map_err(|_| AuthError::Crypto)?;
    let code = format_code(&URL_SAFE_NO_PAD.encode(secret));
    secret.zeroize();
    let expires_unix_seconds = now_seconds()?.saturating_add(lifetime.as_secs());
    let record = PairingRecord {
        id,
        secret: code.replace('-', ""),
        expires_unix_seconds,
    };
    fs::create_dir_all(root.join(PAIRINGS_DIR))?;
    write_private(
        &pairing_path(root, id),
        &serde_json::to_vec_pretty(&record)
            .map_err(|error| AuthError::InvalidData(error.to_string()))?,
    )?;
    Ok(NewPairing {
        id,
        code,
        expires_unix_seconds,
    })
}

pub fn load_pairing(root: &Path, id: Uuid) -> Result<PairingSecret, AuthError> {
    let record: PairingRecord =
        read_json(&pairing_path(root, id)).map_err(|error| match error {
            AuthError::Io(io) if io.kind() == std::io::ErrorKind::NotFound => {
                AuthError::PairingNotFound
            }
            other => other,
        })?;
    if record.id != id {
        return Err(AuthError::InvalidData("pairing identifier mismatch".into()));
    }
    if record.expires_unix_seconds < now_seconds()? {
        let _ = fs::remove_file(pairing_path(root, id));
        return Err(AuthError::PairingExpired);
    }
    let normalized = record.secret.replace('-', "");
    let bytes = URL_SAFE_NO_PAD
        .decode(normalized)
        .map_err(|_| AuthError::InvalidData("invalid pairing secret".into()))?;
    Ok(PairingSecret(bytes))
}

pub fn consume_pairing(root: &Path, id: Uuid) -> Result<(), AuthError> {
    fs::remove_file(pairing_path(root, id)).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            AuthError::PairingNotFound
        } else {
            error.into()
        }
    })
}

pub fn parse_pairing_code(code: &str) -> Result<PairingSecret, AuthError> {
    let normalized = code.trim().replace('-', "");
    let bytes = URL_SAFE_NO_PAD
        .decode(normalized)
        .map_err(|_| AuthError::InvalidData("invalid pairing code".into()))?;
    if bytes.len() != 20 {
        return Err(AuthError::InvalidData("invalid pairing code length".into()));
    }
    Ok(PairingSecret(bytes))
}

pub fn certificate_fingerprint(certificate_der: &[u8]) -> [u8; 32] {
    Sha256::digest(certificate_der).into()
}

fn validate_username(username: &str) -> Result<(), AuthError> {
    let valid = (1..=64).contains(&username.len())
        && username
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
    if valid {
        Ok(())
    } else {
        Err(AuthError::InvalidData(
            "username must be 1-64 ASCII letters, digits, '.', '_' or '-'".into(),
        ))
    }
}

fn hash_password(password: &[u8]) -> Result<String, AuthError> {
    let mut salt_bytes = [0u8; 16];
    getrandom::fill(&mut salt_bytes).map_err(|_| AuthError::Crypto)?;
    let salt = SaltString::encode_b64(&salt_bytes).map_err(|_| AuthError::Crypto)?;
    salt_bytes.zeroize();
    Argon2::default()
        .hash_password(password, &salt)
        .map(|hash| hash.to_string())
        .map_err(|_| AuthError::Crypto)
}

fn pairing_path(root: &Path, id: Uuid) -> PathBuf {
    root.join(PAIRINGS_DIR).join(format!("{id}.json"))
}

fn format_code(encoded: &str) -> String {
    encoded
        .as_bytes()
        .chunks(4)
        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
        .collect::<Vec<_>>()
        .join("-")
}

fn now_seconds() -> Result<u64, AuthError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| AuthError::InvalidData("system clock is before Unix epoch".into()))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, AuthError> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(|error| AuthError::InvalidData(error.to_string()))
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), AuthError> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| AuthError::InvalidData(error.to_string()))?;
    write_private(path, &bytes)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), AuthError> {
    let parent = path
        .parent()
        .ok_or_else(|| AuthError::InvalidData("state file has no parent directory".into()))?;
    let temporary = parent.join(format!(".quickfs-{}.tmp", Uuid::new_v4()));
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::write(&temporary, bytes)?;
        fs::rename(&temporary, path)?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn users_and_pairings_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        initialize(directory.path(), vec!["localhost".into()]).unwrap();
        add_user(directory.path(), "alice", b"correct horse battery staple").unwrap();
        assert!(verify_user(directory.path(), "alice", b"correct horse battery staple").unwrap());
        assert!(!verify_user(directory.path(), "alice", b"incorrect password").unwrap());
        change_password(directory.path(), "alice", b"a different secure password").unwrap();
        assert!(verify_user(directory.path(), "alice", b"a different secure password").unwrap());
        set_user_enabled(directory.path(), "alice", false).unwrap();
        assert!(!verify_user(directory.path(), "alice", b"a different secure password").unwrap());
        set_user_enabled(directory.path(), "alice", true).unwrap();

        let pairing = create_pairing(directory.path(), Duration::from_secs(60)).unwrap();
        let server_secret = load_pairing(directory.path(), pairing.id).unwrap();
        let client_secret = parse_pairing_code(&pairing.code).unwrap();
        let fingerprint = [7; 32];
        let nonce = [9; 32];
        let proof = server_secret.proof(&fingerprint, &nonce).unwrap();
        assert!(client_secret.verify_proof(&fingerprint, &nonce, &proof));
        consume_pairing(directory.path(), pairing.id).unwrap();
        assert!(matches!(
            load_pairing(directory.path(), pairing.id),
            Err(AuthError::PairingNotFound)
        ));
    }
}
