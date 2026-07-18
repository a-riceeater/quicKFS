// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]

use argon2::{
    Argon2, Params,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fmt, fs,
    io::Read,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

const USERS_FILE: &str = "users.json";
const USERS_LOCK_FILE: &str = ".users.lock";
const PAIRINGS_DIR: &str = "pairings";
const CERT_FILE: &str = "server.crt";
const KEY_FILE: &str = "server.key";
const IDENTITIES_DIR: &str = "identities";
const ACTIVE_IDENTITY_FILE: &str = "active-identity";
const IDENTITY_LOCK_FILE: &str = ".identity.lock";
const MAX_IDENTITY_GENERATIONS: usize = 128;
const PAIRING_SECRET_LENGTH: usize = 20;
const PAIRING_CLIENT_PROOF_DOMAIN: &[u8] = b"quickfs pairing client proof v1\0";
const PAIRING_SERVER_PROOF_DOMAIN: &[u8] = b"quickfs pairing server proof v1\0";
const MAX_CERTIFICATE_PEM_SIZE: usize = 4 * 1024 * 1024;
const MAX_PRIVATE_KEY_PEM_SIZE: usize = 64 * 1024;
const MAX_USER_DATABASE_SIZE: u64 = 16 * 1024 * 1024;
const MAX_PAIRING_RECORD_SIZE: u64 = 4 * 1024;
const MAX_USERS: usize = 65_536;
const MAX_PASSWORD_HASH_LENGTH: usize = 256;
pub const MIN_PASSWORD_LENGTH: usize = 12;
pub const MAX_PASSWORD_LENGTH: usize = 1024;
pub const MAX_PAIRING_LIFETIME: Duration = Duration::from_secs(60 * 60);

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

    /// Resolve the atomically selected identity generation, falling back to
    /// the original root-level identity layout for existing installations.
    pub fn resolve(root: impl Into<PathBuf>) -> Result<Self, AuthError> {
        let root = root.into();
        let active = root.join(ACTIVE_IDENTITY_FILE);
        if !path_exists(&active)? {
            return Ok(Self::new(root));
        }
        validate_private_file(&active)?;
        let value = fs::read(&active)?;
        if value.len() > 64 {
            return Err(AuthError::InvalidData(
                "active identity selector is too large".into(),
            ));
        }
        let value = std::str::from_utf8(&value)
            .map_err(|_| AuthError::InvalidData("active identity selector is not UTF-8".into()))?;
        let generation = Uuid::parse_str(value.trim())
            .map_err(|_| AuthError::InvalidData("active identity selector is invalid".into()))?;
        let generation_root = root.join(IDENTITIES_DIR).join(generation.to_string());
        Ok(Self {
            certificate: generation_root.join(CERT_FILE),
            private_key: generation_root.join(KEY_FILE),
            root,
        })
    }
}

#[derive(Clone, Deserialize, Serialize, Zeroize, ZeroizeOnDrop)]
struct UserRecord {
    username: String,
    password_hash: String,
    enabled: bool,
    #[serde(default)]
    writable: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserAuthorization {
    pub writable: bool,
}

#[derive(Default, Deserialize, Serialize, Zeroize, ZeroizeOnDrop)]
struct UserDatabase {
    users: Vec<UserRecord>,
}

#[derive(Clone, Deserialize, Serialize, Zeroize, ZeroizeOnDrop)]
struct PairingRecord {
    #[zeroize(skip)]
    id: Uuid,
    secret: String,
    #[zeroize(skip)]
    expires_unix_seconds: u64,
}

#[derive(Zeroize, ZeroizeOnDrop)]
pub struct PairingSecret(Vec<u8>);

impl PairingSecret {
    fn proof(
        &self,
        domain: &[u8],
        pairing_id: Uuid,
        certificate_fingerprint: &[u8; 32],
        client_nonce: &[u8; 32],
    ) -> Result<[u8; 32], AuthError> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.0).map_err(|_| AuthError::Crypto)?;
        mac.update(domain);
        mac.update(pairing_id.as_bytes());
        mac.update(certificate_fingerprint);
        mac.update(client_nonce);
        Ok(mac.finalize().into_bytes().into())
    }

    fn verify_proof(
        &self,
        domain: &[u8],
        pairing_id: Uuid,
        certificate_fingerprint: &[u8; 32],
        client_nonce: &[u8; 32],
        proof: &[u8],
    ) -> bool {
        let mut mac = match Hmac::<Sha256>::new_from_slice(&self.0) {
            Ok(mac) => mac,
            Err(_) => return false,
        };
        mac.update(domain);
        mac.update(pairing_id.as_bytes());
        mac.update(certificate_fingerprint);
        mac.update(client_nonce);
        mac.verify_slice(proof).is_ok()
    }

    pub fn client_proof(
        &self,
        pairing_id: Uuid,
        certificate_fingerprint: &[u8; 32],
        client_nonce: &[u8; 32],
    ) -> Result<[u8; 32], AuthError> {
        self.proof(
            PAIRING_CLIENT_PROOF_DOMAIN,
            pairing_id,
            certificate_fingerprint,
            client_nonce,
        )
    }

    pub fn verify_client_proof(
        &self,
        pairing_id: Uuid,
        certificate_fingerprint: &[u8; 32],
        client_nonce: &[u8; 32],
        proof: &[u8],
    ) -> bool {
        self.verify_proof(
            PAIRING_CLIENT_PROOF_DOMAIN,
            pairing_id,
            certificate_fingerprint,
            client_nonce,
            proof,
        )
    }

    pub fn server_proof(
        &self,
        pairing_id: Uuid,
        certificate_fingerprint: &[u8; 32],
        client_nonce: &[u8; 32],
    ) -> Result<[u8; 32], AuthError> {
        self.proof(
            PAIRING_SERVER_PROOF_DOMAIN,
            pairing_id,
            certificate_fingerprint,
            client_nonce,
        )
    }

    pub fn verify_server_proof(
        &self,
        pairing_id: Uuid,
        certificate_fingerprint: &[u8; 32],
        client_nonce: &[u8; 32],
        proof: &[u8],
    ) -> bool {
        self.verify_proof(
            PAIRING_SERVER_PROOF_DOMAIN,
            pairing_id,
            certificate_fingerprint,
            client_nonce,
            proof,
        )
    }
}

#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct NewPairing {
    #[zeroize(skip)]
    pub id: Uuid,
    pub code: String,
    #[zeroize(skip)]
    pub expires_unix_seconds: u64,
}

impl fmt::Debug for NewPairing {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NewPairing")
            .field("id", &self.id)
            .field("code", &"[REDACTED]")
            .field("expires_unix_seconds", &self.expires_unix_seconds)
            .finish()
    }
}

pub fn initialize(root: &Path, server_names: Vec<String>) -> Result<StatePaths, AuthError> {
    if server_names.is_empty() {
        return Err(AuthError::InvalidData(
            "at least one server name is required".into(),
        ));
    }
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(server_names)
            .map_err(|error| AuthError::InvalidData(error.to_string()))?;
    let private_key = Zeroizing::new(signing_key.serialize_pem());
    initialize_with_identity(root, cert.pem().as_bytes(), private_key.as_bytes())
}

/// Initialize authentication state with an externally issued PEM certificate
/// chain and its private key. Callers should cryptographically validate the
/// chain/key pair before invoking this filesystem-only state initializer.
pub fn initialize_with_identity(
    root: &Path,
    certificate_pem: &[u8],
    private_key_pem: &[u8],
) -> Result<StatePaths, AuthError> {
    validate_identity_pem(certificate_pem, private_key_pem)?;

    fs::create_dir_all(root)?;
    secure_directory(root)?;
    let paths = StatePaths::new(root);
    let users = root.join(USERS_FILE);
    let pairings = root.join(PAIRINGS_DIR);
    let active_identity = root.join(ACTIVE_IDENTITY_FILE);
    let identities = root.join(IDENTITIES_DIR);
    if path_exists(&paths.certificate)? || path_exists(&paths.private_key)? || path_exists(&users)?
    {
        return Err(AuthError::InvalidData(format!(
            "server state files already exist in '{}'",
            root.display()
        )));
    }
    if path_exists(&pairings)? && fs::read_dir(&pairings)?.next().transpose()?.is_some() {
        return Err(AuthError::InvalidData(
            "pairing records already exist in the state directory".into(),
        ));
    }
    if path_exists(&active_identity)? || path_exists(&identities)? {
        return Err(AuthError::InvalidData(
            "identity generations already exist in the state directory".into(),
        ));
    }
    fs::create_dir_all(&pairings)?;
    secure_directory(&pairings)?;
    write_private(&paths.private_key, private_key_pem)?;
    write_private(&paths.certificate, certificate_pem)?;
    write_json(&users, &UserDatabase::default())?;
    Ok(paths)
}

/// Install a validated certificate chain/private-key pair as a new identity
/// generation and atomically select it for the next daemon start. Existing
/// generations remain available for administrative recovery and are subject
/// to the same permission checks as the active identity.
pub fn install_identity(
    root: &Path,
    certificate_pem: &[u8],
    private_key_pem: &[u8],
) -> Result<StatePaths, AuthError> {
    validate_identity_pem(certificate_pem, private_key_pem)?;
    validate_server_state(root)?;
    let _lock = lock_identity(root)?;
    let identities = root.join(IDENTITIES_DIR);
    fs::create_dir_all(&identities)?;
    secure_directory(&identities)?;
    let existing = fs::read_dir(&identities)?.try_fold(0usize, |count, entry| {
        entry?;
        Ok::<_, std::io::Error>(count.saturating_add(1))
    })?;
    if existing >= MAX_IDENTITY_GENERATIONS {
        return Err(AuthError::InvalidData(format!(
            "identity generation limit of {MAX_IDENTITY_GENERATIONS} reached"
        )));
    }

    let generation = Uuid::new_v4();
    let generation_root = identities.join(generation.to_string());
    fs::create_dir(&generation_root)?;
    secure_directory(&generation_root)?;
    let paths = StatePaths {
        certificate: generation_root.join(CERT_FILE),
        private_key: generation_root.join(KEY_FILE),
        root: root.to_path_buf(),
    };
    let result: Result<(), AuthError> = (|| {
        write_private(&paths.private_key, private_key_pem)?;
        write_private(&paths.certificate, certificate_pem)?;
        write_private(
            &root.join(ACTIVE_IDENTITY_FILE),
            generation.to_string().as_bytes(),
        )?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&paths.certificate);
        let _ = fs::remove_file(&paths.private_key);
        let _ = fs::remove_dir(&generation_root);
    }
    result?;
    Ok(paths)
}

fn validate_identity_pem(certificate_pem: &[u8], private_key_pem: &[u8]) -> Result<(), AuthError> {
    if certificate_pem.is_empty() || certificate_pem.len() > MAX_CERTIFICATE_PEM_SIZE {
        return Err(AuthError::InvalidData(
            "certificate PEM is empty or exceeds the safety limit".into(),
        ));
    }
    if private_key_pem.is_empty() || private_key_pem.len() > MAX_PRIVATE_KEY_PEM_SIZE {
        return Err(AuthError::InvalidData(
            "private-key PEM is empty or exceeds the safety limit".into(),
        ));
    }
    Ok(())
}

pub fn add_user(root: &Path, username: &str, password: &[u8]) -> Result<(), AuthError> {
    validate_server_state(root)?;
    validate_username(username)?;
    validate_new_password(password)?;
    let _lock = lock_user_database(root)?;
    let path = root.join(USERS_FILE);
    let mut database = read_user_database(&path)?;
    if database.users.iter().any(|user| user.username == username) {
        return Err(AuthError::UserExists(username.into()));
    }
    if database.users.len() >= MAX_USERS {
        return Err(AuthError::InvalidData(
            "user database has reached its account limit".into(),
        ));
    }
    database.users.push(UserRecord {
        username: username.into(),
        password_hash: hash_password(password)?,
        enabled: true,
        writable: false,
    });
    write_json(&path, &database)
}

pub fn change_password(root: &Path, username: &str, password: &[u8]) -> Result<(), AuthError> {
    validate_server_state(root)?;
    validate_username(username)?;
    validate_new_password(password)?;
    let _lock = lock_user_database(root)?;
    let path = root.join(USERS_FILE);
    let mut database = read_user_database(&path)?;
    let user = database
        .users
        .iter_mut()
        .find(|user| user.username == username)
        .ok_or_else(|| AuthError::UserNotFound(username.into()))?;
    user.password_hash = hash_password(password)?;
    write_json(&path, &database)
}

pub fn set_user_enabled(root: &Path, username: &str, enabled: bool) -> Result<(), AuthError> {
    validate_server_state(root)?;
    validate_username(username)?;
    let _lock = lock_user_database(root)?;
    let path = root.join(USERS_FILE);
    let mut database = read_user_database(&path)?;
    let user = database
        .users
        .iter_mut()
        .find(|user| user.username == username)
        .ok_or_else(|| AuthError::UserNotFound(username.into()))?;
    user.enabled = enabled;
    write_json(&path, &database)
}

pub fn set_user_writable(root: &Path, username: &str, writable: bool) -> Result<(), AuthError> {
    validate_server_state(root)?;
    validate_username(username)?;
    let _lock = lock_user_database(root)?;
    let path = root.join(USERS_FILE);
    let mut database = read_user_database(&path)?;
    let user = database
        .users
        .iter_mut()
        .find(|user| user.username == username)
        .ok_or_else(|| AuthError::UserNotFound(username.into()))?;
    user.writable = writable;
    write_json(&path, &database)
}

pub fn remove_user(root: &Path, username: &str) -> Result<(), AuthError> {
    validate_server_state(root)?;
    validate_username(username)?;
    let _lock = lock_user_database(root)?;
    let path = root.join(USERS_FILE);
    let mut database = read_user_database(&path)?;
    let previous = database.users.len();
    database.users.retain(|user| user.username != username);
    if database.users.len() == previous {
        return Err(AuthError::UserNotFound(username.into()));
    }
    write_json(&path, &database)
}

pub fn verify_user(root: &Path, username: &str, password: &[u8]) -> Result<bool, AuthError> {
    Ok(verify_user_authorization(root, username, password)?.is_some())
}

pub fn verify_user_authorization(
    root: &Path,
    username: &str,
    password: &[u8],
) -> Result<Option<UserAuthorization>, AuthError> {
    if validate_username(username).is_err() || password.len() > MAX_PASSWORD_LENGTH {
        return Ok(None);
    }
    let users_path = root.join(USERS_FILE);
    validate_private_file(&users_path)?;
    let database = read_user_database(&users_path)?;
    let Some(user) = database
        .users
        .iter()
        .find(|user| user.username == username && user.enabled)
    else {
        // Equalize the expensive path enough to avoid a trivial username oracle.
        let salt =
            SaltString::encode_b64(b"quickfs-unknown-user-salt").map_err(|_| AuthError::Crypto)?;
        let _ = Argon2::default().hash_password(password, &salt);
        return Ok(None);
    };
    let parsed = parse_password_hash(&user.password_hash).map_err(|_| {
        AuthError::InvalidData(format!("invalid password hash for user '{username}'"))
    })?;
    if Argon2::default().verify_password(password, &parsed).is_ok() {
        Ok(Some(UserAuthorization {
            writable: user.writable,
        }))
    } else {
        Ok(None)
    }
}

pub fn create_pairing(root: &Path, lifetime: Duration) -> Result<NewPairing, AuthError> {
    validate_server_state(root)?;
    if lifetime.as_secs() == 0 {
        return Err(AuthError::InvalidData(
            "pairing lifetime must be at least one second".into(),
        ));
    }
    if lifetime > MAX_PAIRING_LIFETIME {
        return Err(AuthError::InvalidData(
            "pairing lifetime must not exceed one hour".into(),
        ));
    }
    let id = Uuid::new_v4();
    let mut secret = Zeroizing::new([0u8; PAIRING_SECRET_LENGTH]);
    getrandom::fill(secret.as_mut()).map_err(|_| AuthError::Crypto)?;
    let encoded = Zeroizing::new(URL_SAFE_NO_PAD.encode(secret.as_ref()));
    let code = format_code(&encoded);
    let expires_unix_seconds = now_seconds()?.saturating_add(lifetime.as_secs());
    let record = PairingRecord {
        id,
        secret: encoded.to_string(),
        expires_unix_seconds,
    };
    fs::create_dir_all(root.join(PAIRINGS_DIR))?;
    secure_directory(&root.join(PAIRINGS_DIR))?;
    let serialized = Zeroizing::new(
        serde_json::to_vec_pretty(&record)
            .map_err(|error| AuthError::InvalidData(error.to_string()))?,
    );
    write_private(&pairing_path(root, id), &serialized)?;
    Ok(NewPairing {
        id,
        code,
        expires_unix_seconds,
    })
}

pub fn load_pairing(root: &Path, id: Uuid) -> Result<PairingSecret, AuthError> {
    let path = pairing_path(root, id);
    validate_private_file(&path).map_err(|error| match error {
        AuthError::Io(io) if io.kind() == std::io::ErrorKind::NotFound => {
            AuthError::PairingNotFound
        }
        other => other,
    })?;
    let record: PairingRecord =
        read_json(&path, MAX_PAIRING_RECORD_SIZE).map_err(|error| match error {
            AuthError::Io(io) if io.kind() == std::io::ErrorKind::NotFound => {
                AuthError::PairingNotFound
            }
            other => other,
        })?;
    if record.id != id {
        return Err(AuthError::InvalidData("pairing identifier mismatch".into()));
    }
    if record.expires_unix_seconds <= now_seconds()? {
        let _ = fs::remove_file(pairing_path(root, id));
        return Err(AuthError::PairingExpired);
    }
    let normalized = Zeroizing::new(record.secret.clone());
    let bytes = URL_SAFE_NO_PAD
        .decode(normalized.as_bytes())
        .map_err(|_| AuthError::InvalidData("invalid pairing secret".into()))?;
    if bytes.len() != PAIRING_SECRET_LENGTH {
        return Err(AuthError::InvalidData(
            "invalid pairing secret length".into(),
        ));
    }
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
    let normalized = Zeroizing::new(normalize_pairing_code(code)?);
    let bytes = URL_SAFE_NO_PAD
        .decode(normalized.as_bytes())
        .map_err(|_| AuthError::InvalidData("invalid pairing code".into()))?;
    if bytes.len() != PAIRING_SECRET_LENGTH {
        return Err(AuthError::InvalidData("invalid pairing code length".into()));
    }
    Ok(PairingSecret(bytes))
}

pub fn certificate_fingerprint(certificate_der: &[u8]) -> [u8; 32] {
    Sha256::digest(certificate_der).into()
}

/// Rejects state layouts that could expose or redirect security-sensitive
/// files. Initialization creates these paths with the required permissions.
pub fn validate_server_state(root: &Path) -> Result<(), AuthError> {
    validate_private_directory(root)?;
    validate_private_directory(&root.join(PAIRINGS_DIR))?;
    validate_private_file(&root.join(CERT_FILE))?;
    validate_private_file(&root.join(KEY_FILE))?;
    validate_private_file(&root.join(USERS_FILE))?;
    validate_identity_generations(root)?;
    let active = StatePaths::resolve(root.to_path_buf())?;
    validate_private_file(&active.certificate)?;
    validate_private_file(&active.private_key)?;
    Ok(())
}

fn validate_identity_generations(root: &Path) -> Result<(), AuthError> {
    let identities = root.join(IDENTITIES_DIR);
    if !path_exists(&identities)? {
        return Ok(());
    }
    validate_private_directory(&identities)?;
    let mut count = 0usize;
    for entry in fs::read_dir(&identities)? {
        let entry = entry?;
        count = count.saturating_add(1);
        if count > MAX_IDENTITY_GENERATIONS {
            return Err(AuthError::InvalidData(format!(
                "identity generation limit of {MAX_IDENTITY_GENERATIONS} exceeded"
            )));
        }
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            AuthError::InvalidData("identity generation name is not UTF-8".into())
        })?;
        Uuid::parse_str(name)
            .map_err(|_| AuthError::InvalidData("identity generation name is invalid".into()))?;
        let generation = entry.path();
        validate_private_directory(&generation)?;
        validate_private_file(&generation.join(CERT_FILE))?;
        validate_private_file(&generation.join(KEY_FILE))?;
    }
    Ok(())
}

pub fn validate_username(username: &str) -> Result<(), AuthError> {
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

fn validate_new_password(password: &[u8]) -> Result<(), AuthError> {
    if password.len() < MIN_PASSWORD_LENGTH {
        return Err(AuthError::InvalidData(format!(
            "password must contain at least {MIN_PASSWORD_LENGTH} bytes"
        )));
    }
    if password.len() > MAX_PASSWORD_LENGTH {
        return Err(AuthError::InvalidData(format!(
            "password must contain at most {MAX_PASSWORD_LENGTH} bytes"
        )));
    }
    Ok(())
}

fn hash_password(password: &[u8]) -> Result<String, AuthError> {
    let mut salt_bytes = Zeroizing::new([0u8; 16]);
    getrandom::fill(salt_bytes.as_mut()).map_err(|_| AuthError::Crypto)?;
    let salt = SaltString::encode_b64(salt_bytes.as_ref()).map_err(|_| AuthError::Crypto)?;
    Argon2::default()
        .hash_password(password, &salt)
        .map(|hash| hash.to_string())
        .map_err(|_| AuthError::Crypto)
}

fn parse_password_hash(value: &str) -> Result<PasswordHash<'_>, AuthError> {
    if value.len() > MAX_PASSWORD_HASH_LENGTH {
        return Err(AuthError::InvalidData(
            "password hash exceeds its safety limit".into(),
        ));
    }
    let parsed = PasswordHash::new(value)
        .map_err(|_| AuthError::InvalidData("password hash is malformed".into()))?;
    if parsed.algorithm.as_str() != "argon2id" || parsed.version != Some(19) {
        return Err(AuthError::InvalidData(
            "password hash uses an unsupported algorithm or version".into(),
        ));
    }
    let parameters = Params::try_from(&parsed)
        .map_err(|_| AuthError::InvalidData("password hash parameters are invalid".into()))?;
    let expected = Params::new(
        Params::DEFAULT_M_COST,
        Params::DEFAULT_T_COST,
        Params::DEFAULT_P_COST,
        Some(Params::DEFAULT_OUTPUT_LEN),
    )
    .map_err(|_| AuthError::Crypto)?;
    if parameters != expected || parsed.salt.is_none() || parsed.hash.is_none() {
        return Err(AuthError::InvalidData(
            "password hash parameters exceed the supported policy".into(),
        ));
    }
    Ok(parsed)
}

fn pairing_path(root: &Path, id: Uuid) -> PathBuf {
    root.join(PAIRINGS_DIR).join(format!("{id}.json"))
}

fn format_code(encoded: &str) -> String {
    let mut formatted = String::with_capacity(encoded.len() + encoded.len().saturating_sub(1) / 4);
    for (index, chunk) in encoded.as_bytes().chunks(4).enumerate() {
        if index != 0 {
            formatted.push('-');
        }
        formatted.push_str(&String::from_utf8_lossy(chunk));
    }
    formatted
}

fn normalize_pairing_code(code: &str) -> Result<String, AuthError> {
    let code = code.trim();
    let encoded_length =
        base64::encoded_len(PAIRING_SECRET_LENGTH, false).ok_or(AuthError::Crypto)?;
    if code.len() == encoded_length {
        return Ok(code.into());
    }

    let separator_count = encoded_length.saturating_sub(1) / 4;
    if code.len() != encoded_length + separator_count {
        return Err(AuthError::InvalidData("invalid pairing code length".into()));
    }
    let mut normalized = String::with_capacity(encoded_length);
    for (index, byte) in code.bytes().enumerate() {
        if index % 5 == 4 {
            if byte != b'-' {
                normalized.zeroize();
                return Err(AuthError::InvalidData(
                    "invalid pairing code grouping".into(),
                ));
            }
        } else {
            normalized.push(char::from(byte));
        }
    }
    Ok(normalized)
}

fn now_seconds() -> Result<u64, AuthError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| AuthError::InvalidData("system clock is before Unix epoch".into()))
}

fn path_exists(path: &Path) -> Result<bool, AuthError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn secure_directory(path: &Path) -> Result<(), AuthError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() {
        return Err(AuthError::InvalidData(format!(
            "private state directory '{}' is not a real directory",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        validate_current_owner(path, &metadata)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn validate_private_directory(path: &Path) -> Result<(), AuthError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() {
        return Err(AuthError::InvalidData(format!(
            "private state directory '{}' is not a real directory",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        validate_current_owner(path, &metadata)?;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(AuthError::InvalidData(format!(
                "private state directory '{}' must not be accessible by group or other users",
                path.display()
            )));
        }
    }
    Ok(())
}

fn validate_private_file(path: &Path) -> Result<(), AuthError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(AuthError::InvalidData(format!(
            "private state file '{}' is not a regular file",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        validate_current_owner(path, &metadata)?;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(AuthError::InvalidData(format!(
                "private state file '{}' must not be accessible by group or other users",
                path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn validate_current_owner(path: &Path, metadata: &fs::Metadata) -> Result<(), AuthError> {
    use std::os::unix::fs::MetadataExt;

    if metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(AuthError::InvalidData(format!(
            "private state path '{}' must be owned by the effective server user",
            path.display()
        )));
    }
    Ok(())
}

struct IdentityLock {
    path: PathBuf,
    _file: fs::File,
}

impl Drop for IdentityLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn lock_identity(root: &Path) -> Result<IdentityLock, AuthError> {
    let path = root.join(IDENTITY_LOCK_FILE);
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            AuthError::InvalidData("another identity installation is in progress".into())
        } else {
            error.into()
        }
    })?;
    Ok(IdentityLock { path, _file: file })
}

struct UserDatabaseLock {
    path: PathBuf,
    _file: fs::File,
}

impl Drop for UserDatabaseLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn lock_user_database(root: &Path) -> Result<UserDatabaseLock, AuthError> {
    let path = root.join(USERS_LOCK_FILE);
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            AuthError::InvalidData(
                "another user-database administration operation is in progress".into(),
            )
        } else {
            error.into()
        }
    })?;
    Ok(UserDatabaseLock { path, _file: file })
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path, maximum: u64) -> Result<T, AuthError> {
    let file = fs::File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > maximum {
        return Err(AuthError::InvalidData(format!(
            "private state file '{}' is not regular or exceeds its safety limit",
            path.display()
        )));
    }
    let mut bytes = Zeroizing::new(Vec::with_capacity(
        usize::try_from(metadata.len()).unwrap_or(0),
    ));
    file.take(maximum.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > maximum {
        return Err(AuthError::InvalidData(format!(
            "private state file '{}' exceeds its safety limit",
            path.display()
        )));
    }
    serde_json::from_slice(&bytes).map_err(|error| AuthError::InvalidData(error.to_string()))
}

fn read_user_database(path: &Path) -> Result<UserDatabase, AuthError> {
    let database: UserDatabase = read_json(path, MAX_USER_DATABASE_SIZE)?;
    if database.users.len() > MAX_USERS {
        return Err(AuthError::InvalidData(
            "user database contains too many accounts".into(),
        ));
    }
    let mut usernames = HashSet::with_capacity(database.users.len());
    for user in &database.users {
        validate_username(&user.username)?;
        parse_password_hash(&user.password_hash)?;
        if !usernames.insert(user.username.as_str()) {
            return Err(AuthError::InvalidData(
                "user database contains a duplicate username".into(),
            ));
        }
    }
    Ok(database)
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), AuthError> {
    let bytes = Zeroizing::new(
        serde_json::to_vec_pretty(value)
            .map_err(|error| AuthError::InvalidData(error.to_string()))?,
    );
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_USER_DATABASE_SIZE {
        return Err(AuthError::InvalidData(
            "user database exceeds its safety limit".into(),
        ));
    }
    write_private(path, &bytes)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), AuthError> {
    let parent = path
        .parent()
        .ok_or_else(|| AuthError::InvalidData("state file has no parent directory".into()))?;
    let temporary = parent.join(format!(".quickfs-{}.tmp", Uuid::new_v4()));
    #[cfg(unix)]
    let result = (|| {
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
    })();
    #[cfg(not(unix))]
    let result = (|| {
        fs::write(&temporary, bytes)?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
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
        assert_eq!(
            verify_user_authorization(directory.path(), "alice", b"correct horse battery staple")
                .unwrap(),
            Some(UserAuthorization { writable: false })
        );
        set_user_writable(directory.path(), "alice", true).unwrap();
        assert_eq!(
            verify_user_authorization(directory.path(), "alice", b"correct horse battery staple")
                .unwrap(),
            Some(UserAuthorization { writable: true })
        );
        set_user_writable(directory.path(), "alice", false).unwrap();
        assert!(!verify_user(directory.path(), "alice", b"incorrect password").unwrap());
        change_password(directory.path(), "alice", b"a different secure password").unwrap();
        assert!(verify_user(directory.path(), "alice", b"a different secure password").unwrap());
        set_user_enabled(directory.path(), "alice", false).unwrap();
        assert!(!verify_user(directory.path(), "alice", b"a different secure password").unwrap());
        set_user_enabled(directory.path(), "alice", true).unwrap();

        let pairing = create_pairing(directory.path(), Duration::from_secs(60)).unwrap();
        let stored: PairingRecord = read_json(
            &pairing_path(directory.path(), pairing.id),
            MAX_PAIRING_RECORD_SIZE,
        )
        .unwrap();
        assert_eq!(
            stored.secret,
            normalize_pairing_code(&pairing.code).unwrap()
        );
        let server_secret = load_pairing(directory.path(), pairing.id).unwrap();
        let client_secret = parse_pairing_code(&pairing.code).unwrap();
        let fingerprint = [7; 32];
        let nonce = [9; 32];
        let client_proof = client_secret
            .client_proof(pairing.id, &fingerprint, &nonce)
            .unwrap();
        assert!(server_secret.verify_client_proof(pairing.id, &fingerprint, &nonce, &client_proof));
        let proof = server_secret
            .server_proof(pairing.id, &fingerprint, &nonce)
            .unwrap();
        assert!(client_secret.verify_server_proof(pairing.id, &fingerprint, &nonce, &proof));
        consume_pairing(directory.path(), pairing.id).unwrap();
        assert!(matches!(
            load_pairing(directory.path(), pairing.id),
            Err(AuthError::PairingNotFound)
        ));
    }

    #[test]
    fn grouping_preserves_url_safe_hyphens_in_pairing_secret() {
        let encoded = "-AAAAAAAAAAAAAAAAAAAAAAAAAA";
        assert_eq!(encoded.len(), 27);
        let grouped = format_code(encoded);
        assert_eq!(normalize_pairing_code(&grouped).unwrap(), encoded);
        assert!(parse_pairing_code(&grouped).is_ok());
    }

    #[test]
    fn rejects_unsafe_password_and_pairing_limits() {
        let directory = tempfile::tempdir().unwrap();
        initialize(directory.path(), vec!["localhost".into()]).unwrap();
        assert!(add_user(directory.path(), "alice", b"too short").is_err());
        assert!(
            add_user(
                directory.path(),
                "alice",
                &vec![b'x'; MAX_PASSWORD_LENGTH + 1]
            )
            .is_err()
        );
        assert!(
            create_pairing(
                directory.path(),
                MAX_PAIRING_LIFETIME + Duration::from_secs(1)
            )
            .is_err()
        );

        let _lock = lock_user_database(directory.path()).unwrap();
        assert!(set_user_enabled(directory.path(), "alice", false).is_err());

        let hash = hash_password(b"correct horse battery staple").unwrap();
        assert!(parse_password_hash(&hash).is_ok());
        let excessive_memory = hash.replace("m=19456", "m=4294967295");
        assert!(parse_password_hash(&excessive_memory).is_err());
    }

    #[test]
    fn identity_generations_switch_through_an_atomic_selector() {
        let directory = tempfile::tempdir().unwrap();
        initialize(directory.path(), vec!["localhost".into()]).unwrap();
        let legacy = StatePaths::resolve(directory.path().to_path_buf()).unwrap();
        assert_eq!(legacy.certificate, directory.path().join(CERT_FILE));

        let installed = install_identity(
            directory.path(),
            b"replacement certificate",
            b"replacement private key",
        )
        .unwrap();
        let active = StatePaths::resolve(directory.path().to_path_buf()).unwrap();
        assert_eq!(active.certificate, installed.certificate);
        assert_eq!(active.private_key, installed.private_key);
        assert_eq!(
            fs::read(&active.certificate).unwrap(),
            b"replacement certificate"
        );
        validate_server_state(directory.path()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn creates_private_state_and_rejects_symlinked_root() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state");
        initialize(&state, vec!["localhost".into()]).unwrap();
        assert_eq!(
            fs::metadata(&state).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(state.join(KEY_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(state.join(CERT_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let target = directory.path().join("target");
        fs::create_dir(&target).unwrap();
        let linked = directory.path().join("linked");
        symlink(&target, &linked).unwrap();
        assert!(initialize(&linked, vec!["localhost".into()]).is_err());
        assert!(!target.join(KEY_FILE).exists());
    }
}
