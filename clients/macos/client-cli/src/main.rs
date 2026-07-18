// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use quickfs_client_core::{
    NetworkFilesystem, RemoteFilesystem, ServerTrust, load_trusted_server_pin, resolve_path,
    verify_pairing,
};
use quickfs_transport_quic::{PairingClient, certificate_sha256, load_certificates};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    io::Read,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

const TRUST_LOCK_FILE: &str = ".trusted-servers.lock";
const MAX_TRUST_DATABASE_SIZE: u64 = 4 * 1024 * 1024;
const MAX_TRUSTED_SERVERS: usize = 4_096;

#[derive(Parser)]
#[command(name = "client-cli")]
struct Cli {
    #[arg(long, env = "QUICKFS_SERVER", default_value = "127.0.0.1:4433")]
    server: SocketAddr,
    #[arg(long, default_value = "localhost")]
    server_name: String,
    #[arg(
        long,
        env = "QUICKFS_CLIENT_STATE_DIR",
        default_value = ".quickfs-client"
    )]
    state_dir: PathBuf,
    #[arg(long, env = "QUICKFS_USERNAME")]
    username: Option<String>,
    #[arg(long, default_value_t = 30_000)]
    timeout_ms: u64,
    /// Validate the server with the operating system's public/managed roots.
    #[arg(long, env = "QUICKFS_TRUST_SYSTEM_ROOTS", conflicts_with = "ca_cert")]
    trust_system_roots: bool,
    /// Validate the server with this PEM enterprise-CA bundle.
    #[arg(
        long,
        env = "QUICKFS_CA_CERT",
        value_name = "PEM",
        conflicts_with = "trust_system_roots"
    )]
    ca_cert: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Pair this client with a server and pin its identity.
    Pair {
        #[arg(long)]
        pairing_id: Uuid,
        #[arg(long, env = "QUICKFS_PAIRING_CODE")]
        code: Option<String>,
    },
    /// Remove a pinned identity so deliberate re-pairing can occur.
    Forget,
    /// Manage exact certificate pins without interactive pairing.
    Trust {
        #[command(subcommand)]
        command: TrustSubcommand,
    },
    Ping,
    List {
        path: String,
    },
    Stat {
        path: String,
    },
    Read {
        path: String,
        #[arg(long, default_value_t = 0)]
        offset: u64,
        #[arg(long)]
        length: u64,
    },
}

#[derive(Subcommand)]
enum TrustSubcommand {
    /// Import a centrally distributed exact certificate fingerprint.
    Import {
        /// SHA-256 fingerprint as 64 hexadecimal digits (colons are accepted).
        #[arg(
            long,
            required_unless_present = "certificate",
            conflicts_with = "certificate"
        )]
        sha256: Option<String>,
        /// PEM certificate or chain whose first certificate will be pinned.
        #[arg(
            long,
            value_name = "PEM",
            required_unless_present = "sha256",
            conflicts_with = "sha256"
        )]
        certificate: Option<PathBuf>,
    },
}

impl Drop for Command {
    fn drop(&mut self) {
        if let Self::Pair {
            code: Some(code), ..
        } = self
        {
            code.zeroize();
        }
    }
}

#[derive(Default, Deserialize, Serialize)]
struct TrustDatabase {
    servers: Vec<TrustedServer>,
}

#[derive(Deserialize, Serialize)]
struct TrustedServer {
    address: SocketAddr,
    server_name: String,
    certificate_sha256: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    quickfs_macos_support::require_macfuse()?;
    let timeout = Duration::from_millis(cli.timeout_ms);
    match &cli.command {
        Command::Pair { pairing_id, code } => {
            reject_enterprise_flags(&cli)?;
            pair(&cli, *pairing_id, code.clone(), timeout).await
        }
        Command::Forget => {
            reject_enterprise_flags(&cli)?;
            forget(&cli.state_dir, cli.server, &cli.server_name)
        }
        Command::Trust { command } => {
            reject_enterprise_flags(&cli)?;
            manage_trust(&cli, command)
        }
        _ => run_authenticated(&cli, timeout).await,
    }
}

fn reject_enterprise_flags(cli: &Cli) -> Result<()> {
    if cli.trust_system_roots || cli.ca_cert.is_some() {
        bail!("--trust-system-roots and --ca-cert apply only to authenticated client commands");
    }
    Ok(())
}

async fn pair(cli: &Cli, pairing_id: Uuid, code: Option<String>, timeout: Duration) -> Result<()> {
    ensure_unpaired(&cli.state_dir, cli.server, &cli.server_name)?;
    let code = Zeroizing::new(match code {
        Some(code) => code,
        None => rpassword::prompt_password("Pairing code: ")?,
    });
    let _trust_lock = lock_trust_store(&cli.state_dir)?;
    // The prompt can be arbitrarily long, so acquire the writer lock only
    // afterward and recheck before consuming the server-side pairing record.
    ensure_unpaired(&cli.state_dir, cli.server, &cli.server_name)?;
    let transport = PairingClient::connect(cli.server, &cli.server_name, timeout)
        .await
        .context("failed to open the temporary pairing connection")?;
    let fingerprint = verify_pairing(&transport, pairing_id, &code)
        .await
        .context("server pairing verification failed")?;
    transport.close();
    save_trust_locked(&cli.state_dir, cli.server, &cli.server_name, fingerprint)?;
    println!("Paired with {} ({})", cli.server_name, cli.server);
    println!("Pinned certificate SHA-256: {}", hex::encode(fingerprint));
    Ok(())
}

async fn run_authenticated(cli: &Cli, timeout: Duration) -> Result<()> {
    let trust = server_trust_from_cli(cli)?;
    let username = cli
        .username
        .clone()
        .ok_or_else(|| anyhow::anyhow!("--username or QUICKFS_USERNAME is required"))?;
    let identity_check = trust
        .connect(cli.server, &cli.server_name, timeout)
        .await
        .with_context(|| format!("failed to authenticate server via {}", trust.description()))?;
    identity_check.close();
    let password = Zeroizing::new(rpassword::prompt_password("Password: ")?);
    // Reconnect after the potentially long interactive prompt so the server's
    // QUIC idle timeout cannot invalidate the authenticated channel. The pin is
    // checked again before any credential is transmitted.
    let transport = trust
        .connect(cli.server, &cli.server_name, timeout)
        .await
        .with_context(|| {
            format!(
                "failed to re-authenticate server via {}; password was not sent",
                trust.description()
            )
        })?;
    let filesystem = NetworkFilesystem::authenticate(transport, username, password.to_string())
        .await
        .context("username/password authentication failed")?;

    match &cli.command {
        Command::Pair { .. } => bail!("pairing command reached authenticated dispatch"),
        Command::Forget => bail!("forget command reached authenticated dispatch"),
        Command::Trust { .. } => bail!("trust command reached authenticated dispatch"),
        Command::Ping => println!("pong {}", filesystem.ping(42).await?),
        Command::List { path } => {
            let node = resolve_path(&filesystem, path).await?;
            for entry in filesystem.list_directory(node).await? {
                println!("{:?}\t{}", entry.kind, entry.name);
            }
        }
        Command::Stat { path } => {
            let node = resolve_path(&filesystem, path).await?;
            println!("{:#?}", filesystem.get_metadata(node).await?);
        }
        Command::Read {
            path,
            offset,
            length,
        } => {
            let node = resolve_path(&filesystem, path).await?;
            let (handle, _, _) = filesystem.open_file(node).await?;
            let result = filesystem.read_range(handle, *offset, *length).await;
            filesystem
                .close_file(handle)
                .await
                .context("closing file")?;
            let bytes = result?;
            use std::io::Write;
            std::io::stdout().write_all(&bytes)?;
        }
    }
    Ok(())
}

fn server_trust_from_cli(cli: &Cli) -> Result<ServerTrust> {
    if cli.trust_system_roots {
        return Ok(ServerTrust::system_roots());
    }
    if let Some(path) = &cli.ca_cert {
        let authorities = load_certificates(path)
            .with_context(|| format!("failed to load enterprise CA bundle '{}'", path.display()))?;
        return Ok(ServerTrust::enterprise_ca(authorities));
    }
    let fingerprint = load_trust(&cli.state_dir, cli.server, &cli.server_name).context(
        "no exact pin is configured; pair first, import a managed pin, or use \
         --ca-cert/--trust-system-roots for centrally managed PKI",
    )?;
    Ok(ServerTrust::pinned(fingerprint))
}

fn manage_trust(cli: &Cli, command: &TrustSubcommand) -> Result<()> {
    match command {
        TrustSubcommand::Import {
            sha256,
            certificate,
        } => {
            let fingerprint = match (sha256, certificate) {
                (Some(fingerprint), None) => parse_sha256_fingerprint(fingerprint)?,
                (None, Some(path)) => {
                    let certificates = load_certificates(path).with_context(|| {
                        format!("failed to load certificate '{}'", path.display())
                    })?;
                    certificate_sha256(
                        certificates
                            .first()
                            .ok_or_else(|| anyhow::anyhow!("certificate bundle is empty"))?,
                    )
                }
                _ => bail!("supply exactly one of --sha256 or --certificate"),
            };
            import_trust(&cli.state_dir, cli.server, &cli.server_name, fingerprint)
        }
    }
}

fn parse_sha256_fingerprint(value: &str) -> Result<[u8; 32]> {
    let normalized: String = value
        .chars()
        .filter(|character| *character != ':' && !character.is_ascii_whitespace())
        .collect();
    if normalized.len() != 64 || !normalized.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("SHA-256 fingerprint must contain exactly 64 hexadecimal digits");
    }
    let decoded = hex::decode(normalized)?;
    decoded
        .try_into()
        .map_err(|_| anyhow::anyhow!("SHA-256 fingerprint has the wrong length"))
}

fn import_trust(
    root: &Path,
    address: SocketAddr,
    server_name: &str,
    fingerprint: [u8; 32],
) -> Result<()> {
    let _trust_lock = lock_trust_store(root)?;
    ensure_unpaired(root, address, server_name)?;
    save_trust_locked(root, address, server_name, fingerprint)?;
    println!("Imported managed pin for {server_name} ({address}).");
    println!("Pinned certificate SHA-256: {}", hex::encode(fingerprint));
    Ok(())
}

fn forget(root: &Path, address: SocketAddr, server_name: &str) -> Result<()> {
    let _trust_lock = lock_trust_store(root)?;
    let path = trust_path(root);
    let mut database = read_trust_database(root, &path)?;
    let previous = database.servers.len();
    database
        .servers
        .retain(|record| record.address != address || record.server_name != server_name);
    if database.servers.len() == previous {
        bail!("no pinned identity exists for {server_name} at {address}");
    }
    write_private_json(&path, &database)?;
    println!("Removed pinned identity for {server_name} ({address}).");
    println!("A new connection will require pairing or another managed trust method.");
    Ok(())
}

fn trust_path(root: &Path) -> PathBuf {
    root.join("trusted-servers.json")
}

fn ensure_unpaired(root: &Path, address: SocketAddr, server_name: &str) -> Result<()> {
    let path = trust_path(root);
    if !path.exists() {
        return Ok(());
    }
    let database = read_trust_database(root, &path)?;
    if database
        .servers
        .iter()
        .any(|record| record.address == address && record.server_name == server_name)
    {
        bail!(
            "this server already has a pinned identity; use `forget` before deliberate re-pairing"
        );
    }
    Ok(())
}

fn load_trust(root: &Path, address: SocketAddr, server_name: &str) -> Result<[u8; 32]> {
    load_trusted_server_pin(root, address, server_name).map_err(Into::into)
}

fn save_trust_locked(
    root: &Path,
    address: SocketAddr,
    server_name: &str,
    fingerprint: [u8; 32],
) -> Result<()> {
    if server_name.is_empty() || server_name.len() > 253 {
        bail!("server name must contain between 1 and 253 bytes");
    }
    fs::create_dir_all(root)?;
    secure_directory(root)?;
    let path = trust_path(root);
    let mut database = if path.exists() {
        read_trust_database(root, &path)?
    } else {
        TrustDatabase::default()
    };
    if let Some(record) = database
        .servers
        .iter()
        .find(|record| record.address == address && record.server_name == server_name)
    {
        let existing: [u8; 32] = hex::decode(&record.certificate_sha256)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("pinned certificate fingerprint has the wrong length"))?;
        if existing != fingerprint {
            bail!(
                "a different identity is already pinned for this server; remove it explicitly before re-pairing"
            );
        }
        return Ok(());
    }
    if database.servers.len() >= MAX_TRUSTED_SERVERS {
        bail!("client trust database has reached its server limit");
    }
    database.servers.push(TrustedServer {
        address,
        server_name: server_name.into(),
        certificate_sha256: hex::encode(fingerprint),
    });
    write_private_json(&path, &database)
}

#[cfg(test)]
fn save_trust(
    root: &Path,
    address: SocketAddr,
    server_name: &str,
    fingerprint: [u8; 32],
) -> Result<()> {
    let _trust_lock = lock_trust_store(root)?;
    save_trust_locked(root, address, server_name, fingerprint)
}

fn write_private_json(path: &Path, value: &TrustDatabase) -> Result<()> {
    validate_trust_database(value)?;
    let bytes = serde_json::to_vec_pretty(value)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_TRUST_DATABASE_SIZE {
        bail!("client trust database exceeds its safety limit");
    }
    let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    #[cfg(unix)]
    let result = (|| -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    #[cfg(not(unix))]
    let result = fs::write(&temporary, bytes).map_err(Into::into);
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

fn secure_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() {
        bail!("client state path is not a real directory");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        validate_current_owner(path, &metadata)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

struct TrustStoreLock {
    path: PathBuf,
    _file: fs::File,
}

impl Drop for TrustStoreLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn lock_trust_store(root: &Path) -> Result<TrustStoreLock> {
    fs::create_dir_all(root)?;
    secure_directory(root)?;
    let path = root.join(TRUST_LOCK_FILE);
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            anyhow::anyhow!("another client trust-store operation is in progress")
        } else {
            error.into()
        }
    })?;
    Ok(TrustStoreLock { path, _file: file })
}

fn validate_trust_store(root: &Path, path: &Path) -> Result<()> {
    let root_metadata =
        fs::symlink_metadata(root).context("client state directory does not exist")?;
    if !root_metadata.file_type().is_dir() {
        bail!("client state path is not a real directory");
    }
    let file_metadata =
        fs::symlink_metadata(path).context("client trust database does not exist")?;
    if !file_metadata.file_type().is_file() {
        bail!("client trust database is not a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        validate_current_owner(root, &root_metadata)?;
        validate_current_owner(path, &file_metadata)?;
        if root_metadata.permissions().mode() & 0o077 != 0 {
            bail!("client state directory must not be accessible by group or other users");
        }
        if file_metadata.permissions().mode() & 0o077 != 0 {
            bail!("client trust database must not be accessible by group or other users");
        }
    }
    Ok(())
}

fn read_trust_database(root: &Path, path: &Path) -> Result<TrustDatabase> {
    validate_trust_store(root, path)?;
    let file = fs::File::open(path).context("client trust database does not exist")?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_TRUST_DATABASE_SIZE {
        bail!("client trust database is not regular or exceeds its safety limit");
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.take(MAX_TRUST_DATABASE_SIZE.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_TRUST_DATABASE_SIZE {
        bail!("client trust database exceeds its safety limit");
    }
    let database: TrustDatabase =
        serde_json::from_slice(&bytes).context("client trust database is malformed")?;
    validate_trust_database(&database)?;
    Ok(database)
}

fn validate_trust_database(database: &TrustDatabase) -> Result<()> {
    if database.servers.len() > MAX_TRUSTED_SERVERS {
        bail!("client trust database contains too many servers");
    }
    let mut identities = HashSet::with_capacity(database.servers.len());
    for record in &database.servers {
        if record.server_name.is_empty() || record.server_name.len() > 253 {
            bail!("client trust database contains an invalid server name");
        }
        let fingerprint = &record.certificate_sha256;
        if fingerprint.len() != 64 || !fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("client trust database contains an invalid certificate fingerprint");
        }
        if !identities.insert((record.address, record.server_name.clone())) {
            bail!("client trust database contains a duplicate server identity");
        }
    }
    Ok(())
}

#[cfg(unix)]
fn validate_current_owner(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    if metadata.uid() != rustix::process::geteuid().as_raw() {
        bail!(
            "private client state path '{}' must be owned by the effective client user",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn trust_store_never_replaces_an_identity() {
        let directory = tempfile::tempdir().unwrap();
        let address = "127.0.0.1:4433".parse().unwrap();
        save_trust(directory.path(), address, "localhost", [7; 32]).unwrap();
        assert_eq!(
            load_trust(directory.path(), address, "localhost").unwrap(),
            [7; 32]
        );
        assert!(save_trust(directory.path(), address, "localhost", [8; 32]).is_err());
        assert!(ensure_unpaired(directory.path(), address, "localhost").is_err());
    }

    #[test]
    fn managed_pin_import_is_strict_and_non_replacing() {
        let directory = tempfile::tempdir().unwrap();
        let address = "127.0.0.1:4433".parse().unwrap();
        let colon_separated = std::iter::repeat_n("07", 32).collect::<Vec<_>>().join(":");
        let fingerprint = parse_sha256_fingerprint(&colon_separated).unwrap();
        assert_eq!(fingerprint, [7; 32]);
        import_trust(directory.path(), address, "localhost", fingerprint).unwrap();
        assert_eq!(
            load_trust(directory.path(), address, "localhost").unwrap(),
            [7; 32]
        );
        assert!(import_trust(directory.path(), address, "localhost", [8; 32]).is_err());
        assert!(parse_sha256_fingerprint("abcd").is_err());
    }

    #[test]
    fn enterprise_trust_arguments_are_mutually_exclusive() {
        assert!(
            Cli::try_parse_from([
                "client-cli",
                "--trust-system-roots",
                "--ca-cert",
                "root.pem",
                "ping"
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["client-cli", "trust", "import"]).is_err());
        assert!(
            Cli::try_parse_from([
                "client-cli",
                "trust",
                "import",
                "--sha256",
                &"07".repeat(32),
                "--certificate",
                "server.pem"
            ])
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn trust_store_is_private_and_rejects_symlinked_root() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state");
        let address = "127.0.0.1:4433".parse().unwrap();
        save_trust(&state, address, "localhost", [7; 32]).unwrap();
        assert_eq!(
            fs::metadata(&state).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(trust_path(&state))
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
        assert!(save_trust(&linked, address, "localhost", [9; 32]).is_err());
        assert!(!trust_path(&target).exists());
    }
}
