// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use quickfs_client_core::{
    MAX_CLIENT_WRITE_SIZE, NetworkFilesystem, RemoteFilesystem, ServerTrust, TrustStore,
    load_trusted_server_pin, parse_sha256_fingerprint, resolve_path, verify_pairing,
};
use quickfs_protocol::{AttributeChanges, FileAccess, FileOpenOptions, RenameMode};
use quickfs_transport_quic::{PairingClient, certificate_sha256, load_certificates};
use std::{
    fs,
    io::Read,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

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
    Capabilities,
    FilesystemStats,
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
    /// Write a local file into an existing remote file at an arbitrary byte offset.
    Write {
        path: String,
        input: PathBuf,
        #[arg(long, default_value_t = 0)]
        offset: u64,
        #[arg(long, default_value_t = false)]
        sync: bool,
    },
    /// Atomically create a new empty remote file.
    Create {
        path: String,
        #[arg(long, default_value_t = 0o644)]
        mode: u32,
    },
    Truncate {
        path: String,
        size: u64,
    },
    Mkdir {
        path: String,
        #[arg(long, default_value_t = 0o755)]
        mode: u32,
    },
    Remove {
        path: String,
        #[arg(long, default_value_t = false)]
        directory: bool,
    },
    Rename {
        source: String,
        destination: String,
        #[arg(long, default_value_t = false)]
        no_replace: bool,
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
    let store = TrustStore::open(&cli.state_dir)?;
    store.ensure_unpinned(cli.server, &cli.server_name)?;
    let code = Zeroizing::new(match code {
        Some(code) => code,
        None => rpassword::prompt_password("Pairing code: ")?,
    });
    let guard = store.lock()?;
    // The prompt can be arbitrarily long, so acquire the writer lock only
    // afterward and recheck before consuming the server-side pairing record.
    guard
        .store()
        .ensure_unpinned(cli.server, &cli.server_name)?;
    let transport = PairingClient::connect(cli.server, &cli.server_name, timeout)
        .await
        .context("failed to open the temporary pairing connection")?;
    let fingerprint = verify_pairing(&transport, pairing_id, &code)
        .await
        .context("server pairing verification failed")?;
    transport.close();
    guard.pin(cli.server, &cli.server_name, fingerprint)?;
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
        Command::Capabilities => println!("{:#?}", filesystem.capabilities().await?),
        Command::FilesystemStats => println!("{:#?}", filesystem.stat_filesystem().await?),
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
        Command::Write {
            path,
            input,
            offset,
            sync,
        } => {
            let node = resolve_path(&filesystem, path).await?;
            let opened = filesystem
                .open_file_with_options(
                    node,
                    FileOpenOptions {
                        access: FileAccess::ReadWrite,
                        truncate: false,
                        append: false,
                    },
                )
                .await?;
            let result = write_input(&filesystem, opened.handle, input, *offset, *sync).await;
            let close_result = filesystem.close_file(opened.handle).await;
            let written = result?;
            close_result.context("closing file after write")?;
            println!("wrote {written} bytes at offset {offset}");
        }
        Command::Create { path, mode } => {
            let (parent, name) = resolve_parent(&filesystem, path).await?;
            let created = filesystem
                .create_file(
                    parent,
                    name.into(),
                    *mode,
                    FileOpenOptions {
                        access: FileAccess::ReadWrite,
                        truncate: false,
                        append: false,
                    },
                )
                .await?;
            filesystem.sync_file(created.opened.handle, false).await?;
            filesystem.close_file(created.opened.handle).await?;
            filesystem.sync_directory(parent).await?;
            println!("created file {:?}", created.metadata.node);
        }
        Command::Truncate { path, size } => {
            let node = resolve_path(&filesystem, path).await?;
            let metadata = filesystem
                .set_attributes(
                    node,
                    None,
                    AttributeChanges {
                        size: Some(*size),
                        mode: None,
                        accessed_unix_ms: None,
                        modified_unix_ms: None,
                        backup_unix_ms: None,
                    },
                )
                .await?;
            println!(
                "truncated to {} bytes (revision {})",
                metadata.size, metadata.revision
            );
        }
        Command::Mkdir { path, mode } => {
            let (parent, name) = resolve_parent(&filesystem, path).await?;
            let metadata = filesystem
                .create_directory(parent, name.into(), *mode)
                .await?;
            println!("created directory {:?}", metadata.node);
        }
        Command::Remove { path, directory } => {
            let (parent, name) = resolve_parent(&filesystem, path).await?;
            filesystem
                .remove_node(parent, name.into(), *directory)
                .await?;
        }
        Command::Rename {
            source,
            destination,
            no_replace,
        } => {
            let (parent, name) = resolve_parent(&filesystem, source).await?;
            let (new_parent, new_name) = resolve_parent(&filesystem, destination).await?;
            filesystem
                .rename_node(
                    parent,
                    name.into(),
                    new_parent,
                    new_name.into(),
                    if *no_replace {
                        RenameMode::NoReplace
                    } else {
                        RenameMode::Replace
                    },
                )
                .await?;
            filesystem.sync_directory(parent).await?;
            if new_parent != parent {
                filesystem.sync_directory(new_parent).await?;
            }
        }
    }
    Ok(())
}

async fn write_input(
    filesystem: &dyn RemoteFilesystem,
    handle: quickfs_protocol::FileHandle,
    input: &Path,
    offset: u64,
    sync: bool,
) -> Result<u64> {
    let capacity = usize::try_from(MAX_CLIENT_WRITE_SIZE)
        .context("client write limit does not fit this platform")?;
    let mut file = fs::File::open(input)
        .with_context(|| format!("failed to open input '{}'", input.display()))?;
    let mut buffer = vec![0; capacity];
    let mut written = 0_u64;
    loop {
        let amount = file.read(&mut buffer)?;
        if amount == 0 {
            break;
        }
        let position = offset
            .checked_add(written)
            .context("write offset overflows")?;
        let result = filesystem
            .write_range(handle, position, &buffer[..amount])
            .await?;
        if result.written != amount as u64 {
            bail!("server reported a partial write");
        }
        written = written
            .checked_add(result.written)
            .context("written byte count overflows")?;
    }
    filesystem.flush_file(handle, None).await?;
    if sync {
        filesystem.sync_file(handle, false).await?;
    }
    Ok(written)
}

async fn resolve_parent(
    filesystem: &dyn RemoteFilesystem,
    path: &str,
) -> Result<(quickfs_protocol::NodeId, String)> {
    let trimmed = path.trim_end_matches('/');
    let (parent_path, name) = match trimmed.rsplit_once('/') {
        Some(("", name)) => ("/", name),
        Some((parent, name)) => (parent, name),
        None => ("/", trimmed),
    };
    if name.is_empty() || name == "." || name == ".." {
        bail!("path must end in a normal entry name");
    }
    Ok((
        resolve_path(filesystem, parent_path).await?,
        name.to_owned(),
    ))
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

fn import_trust(
    root: &Path,
    address: SocketAddr,
    server_name: &str,
    fingerprint: [u8; 32],
) -> Result<()> {
    let store = TrustStore::open(root)?;
    let guard = store.lock()?;
    guard.store().ensure_unpinned(address, server_name)?;
    guard.pin(address, server_name, fingerprint)?;
    println!("Imported managed pin for {server_name} ({address}).");
    println!("Pinned certificate SHA-256: {}", hex::encode(fingerprint));
    Ok(())
}

fn forget(root: &Path, address: SocketAddr, server_name: &str) -> Result<()> {
    let store = TrustStore::open(root)?;
    store.lock()?.forget(address, server_name)?;
    println!("Removed pinned identity for {server_name} ({address}).");
    println!("A new connection will require pairing or another managed trust method.");
    Ok(())
}

fn load_trust(root: &Path, address: SocketAddr, server_name: &str) -> Result<[u8; 32]> {
    load_trusted_server_pin(root, address, server_name).map_err(Into::into)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn trust_store_never_replaces_an_identity() {
        // The store itself is exercised in `quickfs-client-core`; what this
        // checks is that the CLI's own path still goes through it rather than
        // around it.
        let directory = tempfile::tempdir().unwrap();
        let address = "127.0.0.1:4433".parse().unwrap();
        let store = TrustStore::open(directory.path()).unwrap();
        store
            .lock()
            .unwrap()
            .pin(address, "localhost", [7; 32])
            .unwrap();
        assert_eq!(
            load_trust(directory.path(), address, "localhost").unwrap(),
            [7; 32]
        );
        assert!(
            store
                .lock()
                .unwrap()
                .pin(address, "localhost", [8; 32])
                .is_err()
        );
        assert!(store.ensure_unpinned(address, "localhost").is_err());
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
}
