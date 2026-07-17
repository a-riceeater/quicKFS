// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use quickfs_client_core::{NetworkFilesystem, RemoteFilesystem, resolve_path, verify_pairing};
use quickfs_transport_quic::QuicClient;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;
use zeroize::Zeroize;

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
    let timeout = Duration::from_millis(cli.timeout_ms);
    match &cli.command {
        Command::Pair { pairing_id, code } => pair(&cli, *pairing_id, code.clone(), timeout).await,
        Command::Forget => forget(&cli.state_dir, cli.server, &cli.server_name),
        _ => run_authenticated(&cli, timeout).await,
    }
}

async fn pair(cli: &Cli, pairing_id: Uuid, code: Option<String>, timeout: Duration) -> Result<()> {
    let mut code = match code {
        Some(code) => code,
        None => rpassword::prompt_password("Pairing code: ")?,
    };
    let transport = QuicClient::connect_for_pairing(cli.server, &cli.server_name, timeout)
        .await
        .context("failed to open the temporary pairing connection")?;
    let fingerprint = verify_pairing(&transport, pairing_id, &code)
        .await
        .context("server pairing verification failed")?;
    code.zeroize();
    transport.close();
    save_trust(&cli.state_dir, cli.server, &cli.server_name, fingerprint)?;
    println!("Paired with {} ({})", cli.server_name, cli.server);
    println!("Pinned certificate SHA-256: {}", hex::encode(fingerprint));
    Ok(())
}

async fn run_authenticated(cli: &Cli, timeout: Duration) -> Result<()> {
    let fingerprint = load_trust(&cli.state_dir, cli.server, &cli.server_name).with_context(|| {
        format!(
            "server is not paired; run `client-cli --server {} --server-name {} pair --pairing-id <ID>` first",
            cli.server, cli.server_name
        )
    })?;
    let username = cli
        .username
        .clone()
        .ok_or_else(|| anyhow::anyhow!("--username or QUICKFS_USERNAME is required"))?;
    let mut password = rpassword::prompt_password("Password: ")?;
    let transport = QuicClient::connect_pinned(cli.server, &cli.server_name, fingerprint, timeout)
        .await
        .context("failed to connect to the pinned server identity")?;
    let filesystem = NetworkFilesystem::authenticate(transport, username, password.clone()).await;
    password.zeroize();
    let filesystem = filesystem.context("username/password authentication failed")?;

    match &cli.command {
        Command::Pair { .. } => bail!("pairing command reached authenticated dispatch"),
        Command::Forget => bail!("forget command reached authenticated dispatch"),
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

fn forget(root: &Path, address: SocketAddr, server_name: &str) -> Result<()> {
    let path = trust_path(root);
    let mut database: TrustDatabase =
        serde_json::from_slice(&fs::read(&path).context("client trust database does not exist")?)
            .context("client trust database is malformed")?;
    let previous = database.servers.len();
    database
        .servers
        .retain(|record| record.address != address || record.server_name != server_name);
    if database.servers.len() == previous {
        bail!("no pinned identity exists for {server_name} at {address}");
    }
    write_private_json(&path, &database)?;
    println!("Removed pinned identity for {server_name} ({address}).");
    println!("A new connection will require pairing again.");
    Ok(())
}

fn trust_path(root: &Path) -> PathBuf {
    root.join("trusted-servers.json")
}

fn load_trust(root: &Path, address: SocketAddr, server_name: &str) -> Result<[u8; 32]> {
    let database: TrustDatabase = serde_json::from_slice(
        &fs::read(trust_path(root)).context("client trust database does not exist")?,
    )
    .context("client trust database is malformed")?;
    let record = database
        .servers
        .iter()
        .find(|record| record.address == address && record.server_name == server_name)
        .ok_or_else(|| anyhow::anyhow!("no pinned identity for this address and server name"))?;
    let decoded = hex::decode(&record.certificate_sha256)
        .context("pinned certificate fingerprint is malformed")?;
    decoded
        .try_into()
        .map_err(|_| anyhow::anyhow!("pinned certificate fingerprint has the wrong length"))
}

fn save_trust(
    root: &Path,
    address: SocketAddr,
    server_name: &str,
    fingerprint: [u8; 32],
) -> Result<()> {
    fs::create_dir_all(root)?;
    let path = trust_path(root);
    let mut database = if path.exists() {
        serde_json::from_slice(&fs::read(&path)?).context("client trust database is malformed")?
    } else {
        TrustDatabase::default()
    };
    if let Some(record) = database
        .servers
        .iter_mut()
        .find(|record| record.address == address && record.server_name == server_name)
    {
        if record.certificate_sha256 != hex::encode(fingerprint) {
            bail!(
                "a different identity is already pinned for this server; remove it explicitly before re-pairing"
            );
        }
        return Ok(());
    }
    database.servers.push(TrustedServer {
        address,
        server_name: server_name.into(),
        certificate_sha256: hex::encode(fingerprint),
    });
    write_private_json(&path, &database)
}

fn write_private_json(path: &Path, value: &TrustDatabase) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    #[cfg(not(unix))]
    fs::write(&temporary, bytes)?;
    fs::rename(temporary, path)?;
    Ok(())
}
