// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use quickfs_auth::{
    MAX_PASSWORD_LENGTH, StatePaths, add_user, certificate_fingerprint, change_password,
    consume_pairing, create_pairing, initialize, initialize_with_identity, install_identity,
    load_pairing, remove_user, set_user_enabled, set_user_writable, validate_server_state,
    validate_username, verify_user_authorization,
};
use quickfs_common::{DEFAULT_MAX_READ_SIZE, DEFAULT_MAX_WRITE_SIZE, Limits, init_logging};
use quickfs_protocol::*;
use quickfs_server_core::{Export, ExportSession};
use quickfs_transport_quic::{
    RecvStream, SendStream, load_certificate_pem, load_certificates, load_private_key,
    load_private_key_pem, parse_certificates_pem, parse_private_key_pem, read_frame,
    server_endpoint, validate_server_identity, write_frame,
};
use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Semaphore, oneshot};
use zeroize::{Zeroize, Zeroizing};

#[derive(Parser)]
#[command(name = "server-daemon")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Serve(Serve),
    /// Create persistent server identity and authentication state.
    Init(Init),
    /// Manage user accounts.
    User(UserCommand),
    /// Manage one-time client pairing sessions.
    Pair(PairCommand),
    /// Install a renewed or replacement server certificate identity.
    Identity(IdentityCommand),
}

#[derive(clap::Args)]
struct Init {
    #[arg(long, default_value = ".quickfs")]
    state_dir: PathBuf,
    #[arg(
        long = "server-name",
        required_unless_present = "certificate",
        conflicts_with = "certificate"
    )]
    server_names: Vec<String>,
    /// PEM leaf certificate followed by any intermediate certificates.
    #[arg(
        long,
        value_name = "PEM",
        requires = "private_key",
        conflicts_with = "server_names"
    )]
    certificate: Option<PathBuf>,
    /// Unencrypted PEM private key matching the leaf certificate.
    #[arg(
        long,
        value_name = "PEM",
        requires = "certificate",
        conflicts_with = "server_names"
    )]
    private_key: Option<PathBuf>,
}

#[derive(clap::Args)]
struct UserCommand {
    #[command(subcommand)]
    command: UserSubcommand,
}

#[derive(Subcommand)]
enum UserSubcommand {
    Add {
        #[arg(long, default_value = ".quickfs")]
        state_dir: PathBuf,
        username: String,
    },
    Password {
        #[arg(long, default_value = ".quickfs")]
        state_dir: PathBuf,
        username: String,
    },
    Enable {
        #[arg(long, default_value = ".quickfs")]
        state_dir: PathBuf,
        username: String,
    },
    Disable {
        #[arg(long, default_value = ".quickfs")]
        state_dir: PathBuf,
        username: String,
    },
    Delete {
        #[arg(long, default_value = ".quickfs")]
        state_dir: PathBuf,
        username: String,
    },
    /// Permit this user to mutate exports started with --allow-writes.
    GrantWrite {
        #[arg(long, default_value = ".quickfs")]
        state_dir: PathBuf,
        username: String,
    },
    /// Return this user to read-only access.
    RevokeWrite {
        #[arg(long, default_value = ".quickfs")]
        state_dir: PathBuf,
        username: String,
    },
}

#[derive(clap::Args)]
struct PairCommand {
    #[command(subcommand)]
    command: PairSubcommand,
}

#[derive(Subcommand)]
enum PairSubcommand {
    Create {
        #[arg(long, default_value = ".quickfs")]
        state_dir: PathBuf,
        #[arg(long, default_value_t = 300)]
        expires_seconds: u64,
    },
}

#[derive(clap::Args)]
struct IdentityCommand {
    #[command(subcommand)]
    command: IdentitySubcommand,
}

#[derive(Subcommand)]
enum IdentitySubcommand {
    /// Validate and atomically activate an externally issued identity.
    Install {
        #[arg(long, default_value = ".quickfs")]
        state_dir: PathBuf,
        /// PEM leaf certificate followed by any intermediate certificates.
        #[arg(long, value_name = "PEM")]
        certificate: PathBuf,
        /// Unencrypted PEM private key matching the leaf certificate.
        #[arg(long, value_name = "PEM")]
        private_key: PathBuf,
    },
}
#[derive(clap::Args, Clone)]
struct Serve {
    #[arg(long, env = "QUICKFS_BIND", default_value = "0.0.0.0:4433")]
    bind: SocketAddr,
    #[arg(long, env = "QUICKFS_EXPORT_ROOT")]
    export_root: PathBuf,
    #[arg(long, env = "QUICKFS_STATE_DIR", default_value = ".quickfs")]
    state_dir: PathBuf,
    #[arg(long,default_value_t=DEFAULT_MAX_READ_SIZE)]
    max_read_size: u64,
    /// Permit mutating filesystem requests. Exports are read-only by default.
    #[arg(long, env = "QUICKFS_ALLOW_WRITES", default_value_t = false)]
    allow_writes: bool,
    #[arg(long, default_value_t = DEFAULT_MAX_WRITE_SIZE)]
    max_write_size: u64,
    #[arg(long, default_value_t = 1024)]
    max_open_handles: usize,
    #[arg(long, default_value_t = 8_192)]
    max_known_nodes_per_connection: usize,
    #[arg(long, default_value_t = 65_536)]
    max_total_known_nodes: usize,
    /// Aggregate metadata/xattr workers used across directory requests.
    #[arg(long, default_value_t = 64)]
    max_directory_entry_tasks: usize,
    /// End-to-end request deadline, including cold backing-store work and raw
    /// payload transfer. This must comfortably exceed the mount's transport
    /// phase deadline so a slow read is not canceled while still progressing.
    #[arg(long, default_value_t = 120_000)]
    request_timeout_ms: u64,
    #[arg(long, default_value_t = 128)]
    max_concurrent_requests: usize,
    #[arg(long, default_value_t = 128 * 1024 * 1024)]
    max_in_flight_read_bytes: usize,
    /// Aggregate memory reserved for write request bodies across all clients.
    #[arg(long, default_value_t = 64 * 1024 * 1024)]
    max_in_flight_write_bytes: usize,
    /// Memory reserved for write bodies from any one client connection.
    #[arg(long, default_value_t = 16 * 1024 * 1024)]
    max_in_flight_write_bytes_per_connection: usize,
    #[arg(long, default_value_t = 256)]
    max_concurrent_connections: usize,
    #[arg(long, default_value_t = 4)]
    max_concurrent_auth: usize,
    #[arg(long, default_value_t = 30)]
    auth_attempts_per_minute: usize,
}
#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    let Cli { command } = Cli::parse();
    match command {
        Command::Serve(c) => serve(c).await,
        Command::Init(c) => init(c),
        Command::User(c) => manage_user(c),
        Command::Pair(c) => manage_pairing(c),
        Command::Identity(c) => manage_identity(c),
    }
}

fn load_external_identity(
    certificate: &std::path::Path,
    private_key: &std::path::Path,
) -> Result<(Vec<u8>, Zeroizing<Vec<u8>>)> {
    let certificate_pem = load_certificate_pem(certificate).with_context(|| {
        format!(
            "failed to read certificate chain '{}'",
            certificate.display()
        )
    })?;
    let private_key_pem = load_private_key_pem(private_key)
        .with_context(|| format!("failed to read private key '{}'", private_key.display()))?;
    let certificates = parse_certificates_pem(&certificate_pem)
        .context("external certificate chain is invalid")?;
    let key = parse_private_key_pem(&private_key_pem).context("external private key is invalid")?;
    validate_server_identity(certificates, key)
        .context("external certificate and private key are not a valid QUIC identity")?;
    Ok((certificate_pem, private_key_pem))
}

fn manage_identity(command: IdentityCommand) -> Result<()> {
    match command.command {
        IdentitySubcommand::Install {
            state_dir,
            certificate,
            private_key,
        } => {
            let (certificate_pem, private_key_pem) =
                load_external_identity(&certificate, &private_key)?;
            let paths = install_identity(&state_dir, &certificate_pem, &private_key_pem)
                .with_context(|| {
                    format!("failed to install identity in '{}'", state_dir.display())
                })?;
            println!("Installed and selected a new server identity generation:");
            println!("  certificate: {}", paths.certificate.display());
            println!("  private key: {}", paths.private_key.display());
            println!("Restart the daemon to begin presenting the new certificate.");
            Ok(())
        }
    }
}

fn init(command: Init) -> Result<()> {
    let Init {
        state_dir,
        server_names,
        certificate,
        private_key,
    } = command;
    let paths = match (certificate, private_key) {
        (Some(certificate), Some(private_key)) => {
            let (certificate_pem, private_key_pem) =
                load_external_identity(&certificate, &private_key)?;
            initialize_with_identity(&state_dir, &certificate_pem, &private_key_pem)
        }
        (None, None) => initialize(&state_dir, server_names),
        _ => bail!("--certificate and --private-key must be supplied together"),
    }
    .with_context(|| format!("failed to initialize '{}'", state_dir.display()))?;
    println!("Initialized quicKFS server state:");
    println!("  certificate: {}", paths.certificate.display());
    println!("  private key: {}", paths.private_key.display());
    println!("Next, add a user with `server-daemon user add <USERNAME>`.");
    Ok(())
}

fn manage_user(command: UserCommand) -> Result<()> {
    match command.command {
        UserSubcommand::Add {
            state_dir,
            username,
        } => {
            let password = prompt_new_password()?;
            add_user(&state_dir, &username, password.as_bytes())
                .with_context(|| format!("failed to add user '{username}'"))?;
            println!("Added user '{username}'.");
            Ok(())
        }
        UserSubcommand::Password {
            state_dir,
            username,
        } => {
            let password = prompt_new_password()?;
            change_password(&state_dir, &username, password.as_bytes())
                .with_context(|| format!("failed to change password for '{username}'"))?;
            println!("Changed password for '{username}'.");
            Ok(())
        }
        UserSubcommand::Enable {
            state_dir,
            username,
        } => {
            set_user_enabled(&state_dir, &username, true)?;
            println!("Enabled user '{username}'.");
            Ok(())
        }
        UserSubcommand::Disable {
            state_dir,
            username,
        } => {
            set_user_enabled(&state_dir, &username, false)?;
            println!(
                "Disabled user '{username}'. Existing authenticated connections are not revoked."
            );
            Ok(())
        }
        UserSubcommand::Delete {
            state_dir,
            username,
        } => {
            remove_user(&state_dir, &username)?;
            println!(
                "Deleted user '{username}'. Existing authenticated connections are not revoked."
            );
            Ok(())
        }
        UserSubcommand::GrantWrite {
            state_dir,
            username,
        } => {
            set_user_writable(&state_dir, &username, true)?;
            println!(
                "Granted write access to '{username}'. The daemon must also be started with --allow-writes."
            );
            Ok(())
        }
        UserSubcommand::RevokeWrite {
            state_dir,
            username,
        } => {
            set_user_writable(&state_dir, &username, false)?;
            println!(
                "Revoked write access from '{username}'. Existing authenticated connections are not revoked."
            );
            Ok(())
        }
    }
}

fn prompt_new_password() -> Result<Zeroizing<String>> {
    let password = Zeroizing::new(rpassword::prompt_password("Password: ")?);
    let confirmation = Zeroizing::new(rpassword::prompt_password("Confirm password: ")?);
    if password != confirmation {
        bail!("passwords do not match");
    }
    Ok(password)
}

fn manage_pairing(command: PairCommand) -> Result<()> {
    match command.command {
        PairSubcommand::Create {
            state_dir,
            expires_seconds,
        } => {
            let pairing = create_pairing(&state_dir, Duration::from_secs(expires_seconds))?;
            println!("Pairing ID: {}", pairing.id);
            println!("Pairing code: {}", pairing.code);
            println!("Expires at Unix time: {}", pairing.expires_unix_seconds);
            println!(
                "The code is single-use. Transfer it to the client through a trusted channel."
            );
            Ok(())
        }
    }
}

async fn serve(c: Serve) -> Result<()> {
    serve_until(c, shutdown_signal(), None).await
}

async fn serve_until<F>(
    c: Serve,
    shutdown: F,
    ready: Option<oneshot::Sender<SocketAddr>>,
) -> Result<()>
where
    F: Future<Output = ()>,
{
    validate_configuration(&c)?;

    let state_root = tokio::fs::canonicalize(&c.state_dir)
        .await
        .with_context(|| format!("failed to open state directory '{}'", c.state_dir.display()))?;
    let export_root = tokio::fs::canonicalize(&c.export_root)
        .await
        .with_context(|| format!("failed to open export root '{}'", c.export_root.display()))?;
    if state_root.starts_with(&export_root) || export_root.starts_with(&state_root) {
        bail!(
            "server state directory '{}' overlaps export root '{}'; this could expose private authentication state",
            state_root.display(),
            export_root.display()
        );
    }
    validate_server_state(&state_root).context("server state permissions are unsafe")?;

    let state_paths = StatePaths::resolve(state_root.clone())
        .context("failed to resolve the active server identity")?;
    let certificates = load_certificates(&state_paths.certificate).with_context(|| {
        format!(
            "failed to load server identity '{}'; run `server-daemon init` first",
            state_paths.certificate.display()
        )
    })?;
    let leaf = certificates
        .first()
        .ok_or_else(|| anyhow::anyhow!("server certificate chain is empty"))?;
    let fingerprint = certificate_fingerprint(leaf.as_ref());
    let key = load_private_key(&state_paths.private_key).with_context(|| {
        format!(
            "failed to load private key '{}'",
            state_paths.private_key.display()
        )
    })?;
    let export = Arc::new(
        Export::new_persistent_with_writes(
            &export_root,
            state_root.join("filesystem-export.json"),
            Limits {
                max_read_size: c.max_read_size,
                max_write_size: c.max_write_size,
                max_open_handles: c.max_open_handles,
                max_known_nodes: c.max_known_nodes_per_connection,
                max_total_known_nodes: c.max_total_known_nodes,
                max_directory_entry_tasks: c.max_directory_entry_tasks,
                request_timeout_ms: c.request_timeout_ms,
            },
            c.allow_writes,
        )
        .await
        .with_context(|| {
            format!(
                "failed to open export root '{}'; ensure it exists, is a directory, and is readable",
                c.export_root.display()
            )
        })?,
    );
    let endpoint = server_endpoint(c.bind, certificates, key).with_context(|| {
        format!(
            "failed to configure TLS or bind the QUIC server to '{}'",
            c.bind
        )
    })?;
    let auth_root = Arc::new(state_root);
    let permits = Arc::new(Semaphore::new(c.max_concurrent_requests));
    let connection_permits = Arc::new(Semaphore::new(c.max_concurrent_connections));
    let auth_permits = Arc::new(Semaphore::new(c.max_concurrent_auth));
    let read_permits = Arc::new(Semaphore::new(c.max_in_flight_read_bytes));
    let write_permits = Arc::new(Semaphore::new(c.max_in_flight_write_bytes));
    let auth_limiter = Arc::new(Mutex::new(AuthRateLimiter::new(c.auth_attempts_per_minute)));
    let request_timeout = Duration::from_millis(c.request_timeout_ms);
    let local_address = endpoint
        .local_addr()
        .context("failed to determine the bound server address")?;
    if let Some(ready) = ready {
        let _ = ready.send(local_address);
    }
    tracing::info!(address=%local_address,root=%export_root.display(),writable=c.allow_writes,"server listening");
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                let Ok(connection_permit) = connection_permits.clone().try_acquire_owned() else {
                    incoming.refuse();
                    continue;
                };
                let export = export.clone();
                let auth_root = auth_root.clone();
                let permits = permits.clone();
                let auth_permits = auth_permits.clone();
                let auth_limiter = auth_limiter.clone();
                let read_permits = read_permits.clone();
                let write_permits = write_permits.clone();
                let connection_write_budget = c.max_in_flight_write_bytes_per_connection;
                tokio::spawn(async move {
                    let _connection_permit = connection_permit;
                    match incoming.await {
                        Ok(connection) => {
                            let peer_ip = connection.remote_address().ip();
                            let auth_state = Arc::new(Mutex::new(ConnectionAuth::default()));
                            let session = Arc::new(export.session());
                            let connection_write_permits =
                                Arc::new(Semaphore::new(connection_write_budget));
                            let request_context = Arc::new(RequestContext {
                                export: session.clone(),
                                auth_root,
                                fingerprint,
                                auth_state,
                                auth_permits,
                                auth_limiter,
                                read_permits,
                                write_permits,
                                connection_write_permits,
                                peer_ip,
                            });
                            tracing::debug!(%peer_ip, "QUIC connection established");
                            let mut request_tasks = tokio::task::JoinSet::new();
                            loop {
                                let (send, recv) = match connection.accept_bi().await {
                                    Ok(streams) => streams,
                                    Err(error) => {
                                        tracing::debug!(%peer_ip, %error, "QUIC connection ended");
                                        break;
                                    }
                                };
                                let permit = match tokio::time::timeout(
                                    request_timeout,
                                    permits.clone().acquire_owned(),
                                )
                                .await
                                {
                                    Ok(Ok(permit)) => permit,
                                    Ok(Err(_)) => break,
                                    Err(_) => {
                                        tracing::warn!(%peer_ip, "request capacity wait timed out");
                                        continue;
                                    }
                                };
                                let request_context = request_context.clone();
                                request_tasks.spawn(async move {
                                    let _permit = permit;
                                    match tokio::time::timeout(
                                        request_timeout,
                                        handle(send, recv, request_context),
                                    )
                                    .await
                                    {
                                        Ok(Ok(())) => {}
                                        Ok(Err(error)) => tracing::warn!(%error, "request failed"),
                                        Err(_) => tracing::warn!(%peer_ip, "request timed out"),
                                    }
                                });
                                while request_tasks.try_join_next().is_some() {}
                            }
                            request_tasks.abort_all();
                            while request_tasks.join_next().await.is_some() {}
                            if let Err(error) = session.cleanup_locks() {
                                tracing::warn!(%error, %peer_ip, "failed to clean up connection locks");
                            }
                        }
                        Err(error) => tracing::warn!(%error, "connection failed"),
                    }
                });
            }
            _ = &mut shutdown => {
                endpoint.close(0u32.into(), b"server shutdown");
                endpoint.wait_idle().await;
                break;
            }
        }
    }
    Ok(())
}

fn validate_configuration(c: &Serve) -> Result<()> {
    if c.max_read_size == 0 {
        bail!("maximum read size must be greater than zero");
    }
    if c.max_write_size == 0 {
        bail!("maximum write size must be greater than zero");
    }
    if c.max_open_handles == 0 {
        bail!("maximum open handles must be greater than zero");
    }
    if c.max_known_nodes_per_connection == 0 {
        bail!("maximum known nodes per connection must be greater than zero");
    }
    if c.max_total_known_nodes == 0
        || c.max_known_nodes_per_connection.saturating_sub(1) > c.max_total_known_nodes
    {
        bail!("total known-node capacity must cover at least one connection");
    }
    if c.request_timeout_ms == 0 {
        bail!("request timeout must be greater than zero milliseconds");
    }
    if c.max_directory_entry_tasks == 0 {
        bail!("maximum directory entry tasks must be greater than zero");
    }
    if c.max_concurrent_requests == 0 {
        bail!("maximum concurrent requests must be greater than zero");
    }
    if c.max_in_flight_read_bytes == 0 {
        bail!("maximum in-flight read bytes must be greater than zero");
    }
    if c.max_in_flight_write_bytes == 0 || c.max_in_flight_write_bytes_per_connection == 0 {
        bail!("global and per-connection in-flight write-byte budgets must be greater than zero");
    }
    if c.max_read_size > u32::MAX.into()
        || usize::try_from(c.max_read_size).is_err()
        || c.max_read_size as usize > c.max_in_flight_read_bytes
    {
        bail!(
            "maximum read size must fit within the configured in-flight read-byte budget and u32"
        );
    }
    if c.max_write_size > u32::MAX.into()
        || usize::try_from(c.max_write_size).is_err()
        || c.max_write_size as usize > c.max_in_flight_write_bytes
        || c.max_write_size as usize > c.max_in_flight_write_bytes_per_connection
    {
        bail!("maximum write size must fit within both in-flight write-byte budgets and u32");
    }
    if c.max_in_flight_write_bytes_per_connection > c.max_in_flight_write_bytes {
        bail!("per-connection write-byte budget must not exceed the global write-byte budget");
    }
    if c.max_open_handles > Semaphore::MAX_PERMITS
        || c.max_known_nodes_per_connection > Semaphore::MAX_PERMITS
        || c.max_total_known_nodes > Semaphore::MAX_PERMITS
        || c.max_directory_entry_tasks > Semaphore::MAX_PERMITS
        || c.max_concurrent_requests > Semaphore::MAX_PERMITS
        || c.max_concurrent_connections > Semaphore::MAX_PERMITS
        || c.max_concurrent_auth > Semaphore::MAX_PERMITS
        || c.max_in_flight_read_bytes > Semaphore::MAX_PERMITS
        || c.max_in_flight_write_bytes > Semaphore::MAX_PERMITS
        || c.max_in_flight_write_bytes_per_connection > Semaphore::MAX_PERMITS
    {
        bail!("configured concurrency exceeds the runtime semaphore limit");
    }
    if c.max_concurrent_connections == 0 {
        bail!("maximum concurrent connections must be greater than zero");
    }
    if c.max_concurrent_auth == 0 {
        bail!("maximum concurrent authentication tasks must be greater than zero");
    }
    if c.auth_attempts_per_minute == 0 {
        bail!("authentication attempts per minute must be greater than zero");
    }
    if c.auth_attempts_per_minute > 1_000 {
        bail!("authentication attempts per minute must not exceed 1000");
    }
    Ok(())
}

#[derive(Default)]
struct ConnectionAuth {
    authenticated: bool,
    failed_attempts: u8,
}

struct RequestContext {
    export: Arc<ExportSession>,
    auth_root: Arc<PathBuf>,
    fingerprint: [u8; 32],
    auth_state: Arc<Mutex<ConnectionAuth>>,
    auth_permits: Arc<Semaphore>,
    auth_limiter: Arc<Mutex<AuthRateLimiter>>,
    read_permits: Arc<Semaphore>,
    write_permits: Arc<Semaphore>,
    connection_write_permits: Arc<Semaphore>,
    peer_ip: IpAddr,
}

const AUTH_RATE_WINDOW: Duration = Duration::from_secs(60);
const MAX_AUTH_RATE_BUCKETS: usize = 4096;

struct AuthRateLimiter {
    attempts_per_peer: HashMap<IpAddr, VecDeque<Instant>>,
    maximum_attempts: usize,
}

impl AuthRateLimiter {
    fn new(maximum_attempts: usize) -> Self {
        Self {
            attempts_per_peer: HashMap::new(),
            maximum_attempts,
        }
    }

    fn allow(&mut self, peer: IpAddr) -> bool {
        let now = Instant::now();
        let cutoff = now.checked_sub(AUTH_RATE_WINDOW).unwrap_or(now);
        if !self.attempts_per_peer.contains_key(&peer)
            && self.attempts_per_peer.len() >= MAX_AUTH_RATE_BUCKETS
        {
            self.attempts_per_peer.retain(|_, attempts| {
                while attempts.front().is_some_and(|attempt| *attempt <= cutoff) {
                    attempts.pop_front();
                }
                !attempts.is_empty()
            });
            if self.attempts_per_peer.len() >= MAX_AUTH_RATE_BUCKETS {
                return false;
            }
        }
        let attempts = self.attempts_per_peer.entry(peer).or_default();
        while attempts.front().is_some_and(|attempt| *attempt <= cutoff) {
            attempts.pop_front();
        }
        if attempts.len() >= self.maximum_attempts {
            return false;
        }
        attempts.push_back(now);
        true
    }
}

async fn handle(
    mut send: SendStream,
    mut recv: RecvStream,
    context: Arc<RequestContext>,
) -> Result<()> {
    let export = context.export.clone();
    let auth_root = context.auth_root.clone();
    let fingerprint = context.fingerprint;
    let auth_state = context.auth_state.clone();
    let auth_permits = context.auth_permits.clone();
    let auth_limiter = context.auth_limiter.clone();
    let read_permits = context.read_permits.clone();
    let write_permits = context.write_permits.clone();
    let connection_write_permits = context.connection_write_permits.clone();
    let peer_ip = context.peer_ip;
    let request: Envelope<Request> = read_frame(&mut recv).await?;
    let id = request.request_id;
    if request.version != PROTOCOL_VERSION {
        return write_and_finish(
            &mut send,
            id,
            Response::Error(ProtocolError {
                code: ErrorCode::UnsupportedVersion,
                message: "unsupported protocol version".into(),
            }),
        )
        .await;
    }
    let allowed = matches!(
        request.message,
        Request::Hello { .. }
            | Request::Pair { .. }
            | Request::Authenticate { .. }
            | Request::Ping { .. }
    ) || auth_state.lock().await.authenticated;
    if !allowed {
        return write_and_finish(
            &mut send,
            id,
            Response::Error(ProtocolError {
                code: ErrorCode::Unauthenticated,
                message: "authenticate first".into(),
            }),
        )
        .await;
    }
    let mut raw = None;
    let mut read_permit = None;
    let response = match request.message {
        Request::Hello { .. } => Response::HelloAck {
            version: PROTOCOL_VERSION,
        },
        Request::Pair {
            pairing_id,
            client_nonce,
            client_proof,
        } => match load_pairing(&auth_root, pairing_id) {
            Ok(secret)
                if secret.verify_client_proof(
                    pairing_id,
                    &fingerprint,
                    &client_nonce,
                    client_proof.as_bytes(),
                ) =>
            {
                match secret.server_proof(pairing_id, &fingerprint, &client_nonce) {
                    Ok(proof) => match consume_pairing(&auth_root, pairing_id) {
                        Ok(()) => Response::PairingProof {
                            certificate_fingerprint: fingerprint,
                            proof: proof.into(),
                        },
                        Err(error) => {
                            tracing::warn!(%error, %peer_ip, "failed to consume pairing record");
                            authentication_error("pairing session is unavailable")
                        }
                    },
                    Err(error) => {
                        tracing::error!(%error, "failed to create pairing proof");
                        internal_error()
                    }
                }
            }
            Ok(_) => authentication_error("pairing code was not accepted"),
            Err(error) => {
                tracing::warn!(%error, %peer_ip, "pairing request rejected");
                Response::Error(ProtocolError {
                    code: ErrorCode::Unauthenticated,
                    message: "pairing session is unavailable".into(),
                })
            }
        },
        Request::Authenticate {
            username,
            mut password,
        } => {
            let mut state = auth_state.lock().await;
            if state.authenticated {
                password.zeroize();
                Response::Error(ProtocolError {
                    code: ErrorCode::InvalidRequest,
                    message: "connection is already authenticated".into(),
                })
            } else if state.failed_attempts >= 5 {
                password.zeroize();
                authentication_error(
                    "too many failed authentication attempts; reconnect to try again",
                )
            } else if validate_username(&username).is_err()
                || password.is_empty()
                || password.len() > MAX_PASSWORD_LENGTH
            {
                password.zeroize();
                state.failed_attempts = state.failed_attempts.saturating_add(1);
                tracing::warn!(%peer_ip, "authentication input rejected");
                authentication_error("invalid username or password")
            } else if !auth_limiter.lock().await.allow(peer_ip) {
                password.zeroize();
                tracing::warn!(%peer_ip, "authentication rate limit exceeded");
                authentication_error("authentication rate limit exceeded; retry later")
            } else {
                let Ok(auth_permit) = auth_permits.clone().try_acquire_owned() else {
                    password.zeroize();
                    return write_and_finish(
                        &mut send,
                        id,
                        Response::Error(ProtocolError {
                            code: ErrorCode::Timeout,
                            message: "authentication capacity is busy; retry later".into(),
                        }),
                    )
                    .await;
                };
                let root = auth_root.as_ref().clone();
                let log_username = username.clone();
                let password_bytes = Zeroizing::new(password.as_bytes().to_vec());
                password.zeroize();
                let verified = tokio::task::spawn_blocking(move || {
                    let _auth_permit = auth_permit;
                    verify_user_authorization(&root, &username, password_bytes.as_slice())
                })
                .await;
                match verified {
                    Ok(Ok(Some(authorization))) => {
                        export.set_write_authorized(authorization.writable);
                        state.authenticated = true;
                        tracing::info!(
                            username = %log_username,
                            writable = export.capabilities().writable,
                            "user authenticated"
                        );
                        Response::AuthenticateAck
                    }
                    Ok(Ok(None)) => {
                        state.failed_attempts = state.failed_attempts.saturating_add(1);
                        tracing::warn!(username = %log_username, "authentication failed");
                        authentication_error("invalid username or password")
                    }
                    Ok(Err(error)) => {
                        tracing::error!(%error, "authentication backend failed");
                        internal_error()
                    }
                    Err(error) => {
                        tracing::error!(%error, "authentication task failed");
                        internal_error()
                    }
                }
            }
        }
        Request::GetCapabilities => Response::Capabilities(export.capabilities()),
        Request::StatFilesystem => export
            .stat_filesystem()
            .await
            .map(Response::FilesystemStats)
            .unwrap_or_else(|error| Response::Error(error.protocol())),
        Request::GetMetadata { node } => export
            .metadata(node)
            .await
            .map(Response::Metadata)
            .unwrap_or_else(|error| Response::Error(error.protocol())),
        Request::ListDirectory { node } => export
            .list_with_revision(node)
            .await
            .map(|(revision, entries)| Response::DirectoryListing { revision, entries })
            .unwrap_or_else(|error| Response::Error(error.protocol())),
        Request::ListDirectoryView { node, options } => {
            let started = Instant::now();
            let result = export
                .directory_view(node, options)
                .await
                .map(|view| fit_directory_view_response(id, view))
                .unwrap_or_else(|error| Response::Error(error.protocol()));
            tracing::debug!(
                node = %node.0,
                elapsed_ms = started.elapsed().as_millis(),
                "prepared enriched directory view"
            );
            result
        }
        Request::OpenFile { node, options } => export
            .open(node, options)
            .await
            .map(|(handle, revision, size)| Response::FileOpened {
                handle,
                revision,
                size,
            })
            .unwrap_or_else(|error| Response::Error(error.protocol())),
        Request::CreateFile {
            parent,
            name,
            mode,
            options,
        } => export
            .create_file(parent, name.as_bytes(), mode, options)
            .await
            .map(|(metadata, handle, revision, size)| Response::FileCreated {
                metadata,
                handle,
                revision,
                size,
            })
            .unwrap_or_else(|error| Response::Error(error.protocol())),
        Request::CreateDirectory { parent, name, mode } => export
            .create_directory(parent, name.as_bytes(), mode)
            .await
            .map(Response::NodeCreated)
            .unwrap_or_else(|error| Response::Error(error.protocol())),
        Request::CreateSymlink {
            parent,
            name,
            target,
        } => export
            .create_symlink(parent, name.as_bytes(), &target)
            .await
            .map(Response::NodeCreated)
            .unwrap_or_else(|error| Response::Error(error.protocol())),
        Request::RemoveNode {
            parent,
            name,
            directory,
        } => export
            .remove_node(parent, name.as_bytes(), directory)
            .await
            .map(|()| Response::NodeRemoved)
            .unwrap_or_else(|error| Response::Error(error.protocol())),
        Request::RenameNode {
            parent,
            name,
            new_parent,
            new_name,
            mode,
        } => export
            .rename_node(
                parent,
                name.as_bytes(),
                new_parent,
                new_name.as_bytes(),
                mode,
            )
            .await
            .map(|()| Response::NodeRenamed)
            .unwrap_or_else(|error| Response::Error(error.protocol())),
        Request::CreateHardLink {
            node,
            new_parent,
            new_name,
        } => export
            .create_hard_link(node, new_parent, new_name.as_bytes())
            .await
            .map(Response::HardLinkCreated)
            .unwrap_or_else(|error| Response::Error(error.protocol())),
        Request::CreateSpecialNode {
            parent,
            name,
            kind,
            mode,
            device_major,
            device_minor,
        } => export
            .create_special_node(
                parent,
                name.as_bytes(),
                kind,
                mode,
                device_major,
                device_minor,
            )
            .await
            .map(Response::NodeCreated)
            .unwrap_or_else(|error| Response::Error(error.protocol())),
        Request::ReadLink { node } => export
            .read_link(node)
            .await
            .map(Response::LinkTarget)
            .unwrap_or_else(|error| Response::Error(error.protocol())),
        Request::SetAttributes {
            node,
            handle,
            changes,
        } => export
            .set_attributes(node, handle, changes)
            .await
            .map(Response::AttributesChanged)
            .unwrap_or_else(|error| Response::Error(error.protocol())),
        Request::ReadRange {
            handle,
            offset,
            length,
        } => {
            if length > export.capabilities().max_read_size || offset.checked_add(length).is_none()
            {
                Response::Error(ProtocolError {
                    code: ErrorCode::TooLarge,
                    message: "read range exceeds the configured limit".into(),
                })
            } else {
                match u32::try_from(length) {
                    Ok(permit_count) => {
                        match read_permits.clone().acquire_many_owned(permit_count).await {
                            Ok(permit) => match export.read(handle, offset, length).await {
                                Ok((revision, data)) => {
                                    let length = u64::try_from(data.len()).unwrap_or(u64::MAX);
                                    raw = Some(data);
                                    read_permit = Some(permit);
                                    Response::ReadData { revision, length }
                                }
                                Err(error) => Response::Error(error.protocol()),
                            },
                            Err(_) => Response::Error(ProtocolError {
                                code: ErrorCode::Busy,
                                message: "server read capacity is busy; retry later".into(),
                            }),
                        }
                    }
                    Err(_) => Response::Error(ProtocolError {
                        code: ErrorCode::TooLarge,
                        message: "read length exceeds the supported range".into(),
                    }),
                }
            }
        }
        Request::WriteRange {
            handle,
            offset,
            length,
        } => {
            let capabilities = export.capabilities();
            if !capabilities.writable {
                Response::Error(ProtocolError {
                    code: ErrorCode::ReadOnly,
                    message: "the export is read-only".into(),
                })
            } else if length > capabilities.max_write_size || offset.checked_add(length).is_none() {
                Response::Error(ProtocolError {
                    code: ErrorCode::TooLarge,
                    message: "write range exceeds the configured limit".into(),
                })
            } else {
                match (u32::try_from(length), usize::try_from(length)) {
                    (Ok(permit_count), Ok(body_length)) => {
                        match write_permits.clone().try_acquire_many_owned(permit_count) {
                            Ok(global_permit) => match connection_write_permits
                                .clone()
                                .try_acquire_many_owned(permit_count)
                            {
                                Ok(connection_permit) => {
                                    let mut data = vec![0; body_length];
                                    recv.read_exact(&mut data).await?;
                                    let export = export.clone();
                                    // Retain both memory reservations with the owned body if
                                    // the request deadline expires after filesystem work starts.
                                    match tokio::spawn(async move {
                                        let _write_memory = (global_permit, connection_permit);
                                        export.write_owned(handle, offset, data).await
                                    })
                                    .await
                                    {
                                        Ok(Ok((written, revision, size))) => {
                                            Response::WriteComplete {
                                                written,
                                                revision,
                                                size,
                                            }
                                        }
                                        Ok(Err(error)) => Response::Error(error.protocol()),
                                        Err(error) => {
                                            tracing::error!(%error, "write task failed");
                                            internal_error()
                                        }
                                    }
                                }
                                Err(_) => Response::Error(ProtocolError {
                                    code: ErrorCode::Busy,
                                    message: "connection write capacity is busy; retry later"
                                        .into(),
                                }),
                            },
                            Err(_) => Response::Error(ProtocolError {
                                code: ErrorCode::Busy,
                                message: "server write capacity is busy; retry later".into(),
                            }),
                        }
                    }
                    _ => Response::Error(ProtocolError {
                        code: ErrorCode::TooLarge,
                        message: "write length exceeds the supported range".into(),
                    }),
                }
            }
        }
        Request::FlushFile { handle, lock_owner } => export
            .flush(handle, lock_owner)
            .await
            .map(|()| Response::FileFlushed)
            .unwrap_or_else(|error| Response::Error(error.protocol())),
        Request::SyncFile { handle, data_only } => export
            .sync(handle, data_only)
            .await
            .map(|()| Response::FileSynced)
            .unwrap_or_else(|error| Response::Error(error.protocol())),
        Request::SyncDirectory { node } => export
            .sync_directory(node)
            .await
            .map(|()| Response::DirectorySynced)
            .unwrap_or_else(|error| Response::Error(error.protocol())),
        Request::AllocateFile {
            handle,
            offset,
            length,
        } => export
            .allocate(handle, offset, length)
            .await
            .map(|(revision, size)| Response::FileAllocated { revision, size })
            .unwrap_or_else(|error| Response::Error(error.protocol())),
        Request::GetXattr {
            node,
            name,
            offset,
            length,
        } => {
            if length > export.capabilities().max_read_size || offset.checked_add(length).is_none()
            {
                Response::Error(ProtocolError {
                    code: ErrorCode::TooLarge,
                    message: "xattr read exceeds the configured limit".into(),
                })
            } else {
                match u32::try_from(length) {
                    Ok(permit_count) => {
                        match read_permits.clone().acquire_many_owned(permit_count).await {
                            Ok(permit) => match export.get_xattr(node, &name, offset, length).await
                            {
                                Ok((total_size, data)) => {
                                    let length = data.len() as u64;
                                    raw = Some(data);
                                    read_permit = Some(permit);
                                    Response::XattrData { length, total_size }
                                }
                                Err(error) => Response::Error(error.protocol()),
                            },
                            Err(_) => Response::Error(ProtocolError {
                                code: ErrorCode::Busy,
                                message: "server read capacity is busy; retry later".into(),
                            }),
                        }
                    }
                    Err(_) => Response::Error(ProtocolError {
                        code: ErrorCode::TooLarge,
                        message: "xattr read exceeds the supported range".into(),
                    }),
                }
            }
        }
        Request::SetXattr {
            node,
            name,
            mode,
            position,
            length,
        } => {
            let capabilities = export.capabilities();
            if !capabilities.writable {
                Response::Error(ProtocolError {
                    code: ErrorCode::ReadOnly,
                    message: "the export is read-only".into(),
                })
            } else if length > capabilities.max_write_size {
                Response::Error(ProtocolError {
                    code: ErrorCode::TooLarge,
                    message: "xattr write exceeds the configured limit".into(),
                })
            } else {
                match (u32::try_from(length), usize::try_from(length)) {
                    (Ok(permit_count), Ok(body_length)) => {
                        match write_permits.clone().try_acquire_many_owned(permit_count) {
                            Ok(global_permit) => match connection_write_permits
                                .clone()
                                .try_acquire_many_owned(permit_count)
                            {
                                Ok(connection_permit) => {
                                    let mut data = vec![0; body_length];
                                    recv.read_exact(&mut data).await?;
                                    let export = export.clone();
                                    match tokio::spawn(async move {
                                        let _write_memory = (global_permit, connection_permit);
                                        export.set_xattr(node, &name, data, mode, position).await
                                    })
                                    .await
                                    {
                                        Ok(Ok(())) => Response::XattrSet,
                                        Ok(Err(error)) => Response::Error(error.protocol()),
                                        Err(error) => {
                                            tracing::error!(%error, "xattr task failed");
                                            internal_error()
                                        }
                                    }
                                }
                                Err(_) => Response::Error(ProtocolError {
                                    code: ErrorCode::Busy,
                                    message: "connection write capacity is busy; retry later"
                                        .into(),
                                }),
                            },
                            Err(_) => Response::Error(ProtocolError {
                                code: ErrorCode::Busy,
                                message: "server write capacity is busy; retry later".into(),
                            }),
                        }
                    }
                    _ => Response::Error(ProtocolError {
                        code: ErrorCode::TooLarge,
                        message: "xattr write exceeds the supported range".into(),
                    }),
                }
            }
        }
        Request::ListXattrs { node } => export
            .list_xattrs(node)
            .await
            .map(Response::XattrNames)
            .unwrap_or_else(|error| Response::Error(error.protocol())),
        Request::RemoveXattr { node, name } => export
            .remove_xattr(node, &name)
            .await
            .map(|()| Response::XattrRemoved)
            .unwrap_or_else(|error| Response::Error(error.protocol())),
        Request::CopyFileRange {
            input,
            input_offset,
            output,
            output_offset,
            length,
        } => export
            .copy_file_range(input, input_offset, output, output_offset, length)
            .await
            .map(|(copied, revision, size)| Response::RangeCopied {
                copied,
                revision,
                size,
            })
            .unwrap_or_else(|error| Response::Error(error.protocol())),
        Request::SeekFile {
            handle,
            offset,
            whence,
        } => export
            .seek_file(handle, offset, whence)
            .await
            .map(|offset| Response::FileSeeked { offset })
            .unwrap_or_else(|error| Response::Error(error.protocol())),
        Request::SafeIoctl { handle, operation } => export
            .safe_ioctl(handle, operation)
            .map(|value| Response::IoctlResult { value })
            .unwrap_or_else(|error| Response::Error(error.protocol())),
        Request::MapBlock {
            node,
            block_size,
            block,
        } => export
            .map_block(node, block_size, block)
            .await
            .map(|block| Response::BlockMapped { block })
            .unwrap_or_else(|error| Response::Error(error.protocol())),
        Request::ExchangeData {
            parent,
            name,
            new_parent,
            new_name,
            options,
        } => export
            .exchange_data(
                parent,
                name.as_bytes(),
                new_parent,
                new_name.as_bytes(),
                options,
            )
            .await
            .map(|()| Response::DataExchanged)
            .unwrap_or_else(|error| Response::Error(error.protocol())),
        Request::SetVolumeName { name } => export
            .set_volume_name(&name)
            .map(|()| Response::VolumeNameSet)
            .unwrap_or_else(|error| Response::Error(error.protocol())),
        Request::ForgetNodes { nodes } => export
            .forget_nodes(&nodes)
            .map(|()| Response::NodesForgotten)
            .unwrap_or_else(|error| Response::Error(error.protocol())),
        Request::GetLock { handle, lock } => export
            .get_lock(handle, lock)
            .map(|conflict| Response::LockStatus { conflict })
            .unwrap_or_else(|error| Response::Error(error.protocol())),
        Request::SetLock { handle, lock, wait } => export
            .set_lock(handle, lock, wait)
            .await
            .map(|()| Response::LockUpdated)
            .unwrap_or_else(|error| Response::Error(error.protocol())),
        Request::CloseFile { handle } => export
            .close(handle)
            .map(|()| Response::FileClosed)
            .unwrap_or_else(|error| Response::Error(error.protocol())),
        Request::Ping { nonce } => Response::Pong { nonce },
    };
    write_response(&mut send, id, response).await?;
    if let Some(data) = raw {
        send.write_all(&data).await?
    }
    send.finish()?;
    drop(read_permit);
    Ok(())
}
async fn write_response(send: &mut SendStream, id: RequestId, message: Response) -> Result<()> {
    write_frame(
        send,
        &Envelope {
            version: PROTOCOL_VERSION,
            request_id: id,
            message,
        },
    )
    .await?;
    Ok(())
}

async fn write_and_finish(send: &mut SendStream, id: RequestId, message: Response) -> Result<()> {
    write_response(send, id, message).await?;
    send.finish()?;
    Ok(())
}

fn authentication_error(message: &str) -> Response {
    Response::Error(ProtocolError {
        code: ErrorCode::Unauthenticated,
        message: message.into(),
    })
}

fn internal_error() -> Response {
    Response::Error(ProtocolError {
        code: ErrorCode::Internal,
        message: "internal server error".into(),
    })
}

fn fit_directory_view_response(id: RequestId, mut view: DirectoryView) -> Response {
    let fits = |candidate: &DirectoryView| {
        encode(&Envelope {
            version: PROTOCOL_VERSION,
            request_id: id,
            message: Response::DirectoryView(candidate.clone()),
        })
        .is_ok()
    };
    if fits(&view) {
        return Response::DirectoryView(view);
    }

    if let Some(xattrs) = &mut view.xattrs {
        xattrs.inline_values.clear();
    }
    for entry in &mut view.entries {
        if let Some(xattrs) = &mut entry.xattrs {
            xattrs.inline_values.clear();
        }
    }
    if fits(&view) {
        return Response::DirectoryView(view);
    }

    view.xattrs = None;
    for entry in &mut view.entries {
        entry.xattrs = None;
    }
    if fits(&view) {
        Response::DirectoryView(view)
    } else {
        Response::Error(ProtocolError {
            code: ErrorCode::TooLarge,
            message: "directory view exceeds the control-frame limit".into(),
        })
    }
}
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut term) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}};
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use quickfs_client_core::{
        ClientError, NetworkFilesystem, RemoteFilesystem, resolve_path, verify_pairing,
    };
    use quickfs_transport_quic::{PairingClient, QuicClient, parse_certificates_pem};
    use rcgen::{
        BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
        KeyUsagePurpose,
    };
    use tempfile::TempDir;

    struct TestServer {
        _state: TempDir,
        _export: TempDir,
        state_path: PathBuf,
        address: SocketAddr,
        fingerprint: [u8; 32],
        stop: oneshot::Sender<()>,
        task: tokio::task::JoinHandle<Result<()>>,
    }

    impl TestServer {
        async fn start(request_timeout_ms: u64) -> Self {
            let state = tempfile::tempdir().unwrap();
            let export = tempfile::tempdir().unwrap();
            std::fs::write(export.path().join("example.txt"), b"authenticated contents").unwrap();
            initialize(state.path(), vec!["localhost".into()]).unwrap();
            add_user(state.path(), "alice", b"correct horse battery staple").unwrap();
            Self::start_prepared(state, export, request_timeout_ms).await
        }

        async fn start_writable(request_timeout_ms: u64) -> Self {
            let state = tempfile::tempdir().unwrap();
            let export = tempfile::tempdir().unwrap();
            initialize(state.path(), vec!["localhost".into()]).unwrap();
            add_user(state.path(), "alice", b"correct horse battery staple").unwrap();
            set_user_writable(state.path(), "alice", true).unwrap();
            Self::start_prepared_with_write_setting(state, export, request_timeout_ms, true).await
        }

        async fn start_with_identity(
            request_timeout_ms: u64,
            certificate_pem: &[u8],
            private_key_pem: &[u8],
        ) -> Self {
            let state = tempfile::tempdir().unwrap();
            let export = tempfile::tempdir().unwrap();
            std::fs::write(export.path().join("example.txt"), b"authenticated contents").unwrap();
            initialize_with_identity(state.path(), certificate_pem, private_key_pem).unwrap();
            add_user(state.path(), "alice", b"correct horse battery staple").unwrap();
            Self::start_prepared(state, export, request_timeout_ms).await
        }

        async fn start_with_installed_identity(
            request_timeout_ms: u64,
            certificate_pem: &[u8],
            private_key_pem: &[u8],
        ) -> Self {
            let state = tempfile::tempdir().unwrap();
            let export = tempfile::tempdir().unwrap();
            std::fs::write(export.path().join("example.txt"), b"authenticated contents").unwrap();
            initialize(state.path(), vec!["localhost".into()]).unwrap();
            install_identity(state.path(), certificate_pem, private_key_pem).unwrap();
            add_user(state.path(), "alice", b"correct horse battery staple").unwrap();
            Self::start_prepared(state, export, request_timeout_ms).await
        }

        async fn start_prepared(state: TempDir, export: TempDir, request_timeout_ms: u64) -> Self {
            Self::start_prepared_with_write_setting(state, export, request_timeout_ms, false).await
        }

        async fn start_prepared_with_write_setting(
            state: TempDir,
            export: TempDir,
            request_timeout_ms: u64,
            allow_writes: bool,
        ) -> Self {
            let active_identity = StatePaths::resolve(state.path().to_path_buf()).unwrap();
            let certificates = load_certificates(&active_identity.certificate).unwrap();
            let fingerprint = certificate_fingerprint(certificates[0].as_ref());
            let mut config = test_configuration(state.path(), export.path(), request_timeout_ms);
            config.allow_writes = allow_writes;
            let (ready_tx, ready_rx) = oneshot::channel();
            let (stop_tx, stop_rx) = oneshot::channel();
            let task = tokio::spawn(serve_until(
                config,
                async move {
                    let _ = stop_rx.await;
                },
                Some(ready_tx),
            ));
            let address = match ready_rx.await {
                Ok(address) => address,
                Err(_) => panic!("server startup failed: {:?}", task.await.unwrap()),
            };
            Self {
                state_path: state.path().to_path_buf(),
                _state: state,
                _export: export,
                address,
                fingerprint,
                stop: stop_tx,
                task,
            }
        }

        async fn stop(self) {
            let _ = self.stop.send(());
            self.task.await.unwrap().unwrap();
        }

        async fn stop_preserving(self) -> (TempDir, TempDir) {
            let Self {
                _state: state,
                _export: export,
                stop,
                task,
                ..
            } = self;
            let _ = stop.send(());
            task.await.unwrap().unwrap();
            (state, export)
        }

        async fn pinned_client(&self) -> QuicClient {
            QuicClient::connect_pinned(
                self.address,
                "localhost",
                self.fingerprint,
                Duration::from_secs(5),
            )
            .await
            .unwrap()
        }
    }

    fn enterprise_identity(server_name: &str) -> (String, String, String) {
        let mut ca_params = CertificateParams::new(Vec::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages.push(KeyUsagePurpose::DigitalSignature);
        ca_params.key_usages.push(KeyUsagePurpose::KeyCertSign);
        ca_params.key_usages.push(KeyUsagePurpose::CrlSign);
        let ca_key = KeyPair::generate().unwrap();
        let ca_certificate = ca_params.self_signed(&ca_key).unwrap();
        let issuer = Issuer::new(ca_params, ca_key);

        let mut leaf_params = CertificateParams::new(vec![server_name.into()]).unwrap();
        leaf_params.use_authority_key_identifier_extension = true;
        leaf_params
            .key_usages
            .push(KeyUsagePurpose::DigitalSignature);
        leaf_params
            .extended_key_usages
            .push(ExtendedKeyUsagePurpose::ServerAuth);
        let leaf_key = KeyPair::generate().unwrap();
        let leaf_certificate = leaf_params.signed_by(&leaf_key, &issuer).unwrap();
        let chain = format!("{}{}", leaf_certificate.pem(), ca_certificate.pem());
        (ca_certificate.pem(), chain, leaf_key.serialize_pem())
    }

    fn test_configuration(
        state_dir: &std::path::Path,
        export_root: &std::path::Path,
        timeout: u64,
    ) -> Serve {
        Serve {
            bind: "127.0.0.1:0".parse().unwrap(),
            export_root: export_root.to_path_buf(),
            state_dir: state_dir.to_path_buf(),
            max_read_size: DEFAULT_MAX_READ_SIZE,
            allow_writes: false,
            max_write_size: DEFAULT_MAX_WRITE_SIZE,
            max_open_handles: 8,
            max_known_nodes_per_connection: 128,
            max_total_known_nodes: 512,
            max_directory_entry_tasks: 8,
            request_timeout_ms: timeout,
            max_concurrent_requests: 32,
            max_in_flight_read_bytes: 16 * 1024 * 1024,
            max_in_flight_write_bytes: 16 * 1024 * 1024,
            max_in_flight_write_bytes_per_connection: 16 * 1024 * 1024,
            max_concurrent_connections: 32,
            max_concurrent_auth: 2,
            auth_attempts_per_minute: 100,
        }
    }

    async fn request(client: &QuicClient, message: Request) -> Response {
        let envelope = Envelope::new(message);
        let (mut send, mut recv) = client.stream().await.unwrap();
        client.send_frame(&mut send, &envelope).await.unwrap();
        send.finish().unwrap();
        let response: Envelope<Response> = client.receive_frame(&mut recv).await.unwrap();
        assert_eq!(response.version, PROTOCOL_VERSION);
        assert_eq!(response.request_id, envelope.request_id);
        response.message
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn full_authentication_pipeline_and_adversarial_edges() {
        let server = TestServer::start(5_000).await;

        let pairing = create_pairing(&server.state_path, Duration::from_secs(60)).unwrap();
        let pairing_client =
            PairingClient::connect(server.address, "localhost", Duration::from_secs(5))
                .await
                .unwrap();
        assert!(
            verify_pairing(&pairing_client, pairing.id, "AAAAAAAAAAAAAAAAAAAAAAAAAAA")
                .await
                .is_err()
        );
        let fingerprint = verify_pairing(&pairing_client, pairing.id, &pairing.code)
            .await
            .unwrap();
        assert_eq!(fingerprint, server.fingerprint);
        assert!(
            verify_pairing(&pairing_client, pairing.id, &pairing.code)
                .await
                .is_err()
        );
        pairing_client.close();

        let expired = create_pairing(&server.state_path, Duration::from_secs(1)).unwrap();
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        let expired_client =
            PairingClient::connect(server.address, "localhost", Duration::from_secs(5))
                .await
                .unwrap();
        assert!(
            verify_pairing(&expired_client, expired.id, &expired.code)
                .await
                .is_err()
        );
        expired_client.close();

        assert!(
            QuicClient::connect_pinned(
                server.address,
                "localhost",
                [0; 32],
                Duration::from_secs(2),
            )
            .await
            .is_err()
        );

        let unauthenticated = server.pinned_client().await;
        assert!(matches!(
            request(&unauthenticated, Request::ListDirectory { node: ROOT_NODE }).await,
            Response::Error(ProtocolError {
                code: ErrorCode::Unauthenticated,
                ..
            })
        ));
        unauthenticated.close();

        let wrong_password = server.pinned_client().await;
        assert!(
            NetworkFilesystem::authenticate(
                wrong_password,
                "alice".into(),
                "definitely incorrect".into(),
            )
            .await
            .is_err()
        );

        let limited = Arc::new(server.pinned_client().await);
        let mut attempts = Vec::new();
        for attempt in 0..6 {
            let client = limited.clone();
            attempts.push(tokio::spawn(async move {
                request(
                    &client,
                    Request::Authenticate {
                        username: "alice".into(),
                        password: format!("wrong password {attempt}").into(),
                    },
                )
                .await
            }));
        }
        let mut locked_responses = 0;
        for attempt in attempts {
            if let Response::Error(error) = attempt.await.unwrap()
                && error.message.contains("too many failed")
            {
                locked_responses += 1;
            }
        }
        assert_eq!(locked_responses, 1);
        assert!(matches!(
            request(
                &limited,
                Request::Authenticate {
                    username: "alice".into(),
                    password: "correct horse battery staple".to_string().into(),
                },
            )
            .await,
            Response::Error(ProtocolError {
                code: ErrorCode::Unauthenticated,
                ..
            })
        ));
        limited.close();

        let authenticated = NetworkFilesystem::authenticate(
            server.pinned_client().await,
            "alice".into(),
            "correct horse battery staple".into(),
        )
        .await
        .unwrap();
        assert_eq!(authenticated.ping(73).await.unwrap(), 73);
        let node = resolve_path(&authenticated, "/example.txt").await.unwrap();
        let (handle, _, size) = authenticated.open_file(node).await.unwrap();
        assert_eq!(size, 22);
        assert_eq!(
            authenticated.read_range(handle, 0, 100).await.unwrap(),
            b"authenticated contents"
        );
        authenticated.close_file(handle).await.unwrap();

        set_user_enabled(&server.state_path, "alice", false).unwrap();
        assert_eq!(authenticated.ping(74).await.unwrap(), 74);
        assert!(
            NetworkFilesystem::authenticate(
                server.pinned_client().await,
                "alice".into(),
                "correct horse battery staple".into(),
            )
            .await
            .is_err()
        );
        set_user_enabled(&server.state_path, "alice", true).unwrap();
        let reenabled = NetworkFilesystem::authenticate(
            server.pinned_client().await,
            "alice".into(),
            "correct horse battery staple".into(),
        )
        .await
        .unwrap();
        assert_eq!(reenabled.ping(75).await.unwrap(), 75);
        drop(reenabled);
        drop(authenticated);

        server.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn writable_v6_pipeline_supports_random_io_sync_rename_and_connection_locks() {
        let server = TestServer::start_writable(5_000).await;
        let first = NetworkFilesystem::authenticate(
            server.pinned_client().await,
            "alice".into(),
            "correct horse battery staple".into(),
        )
        .await
        .unwrap();
        let capabilities = first.capabilities().await.unwrap();
        assert!(capabilities.writable);
        assert!(capabilities.supports_locks);
        assert!(capabilities.supports_atomic_rename);
        assert_eq!(capabilities.max_write_size, DEFAULT_MAX_WRITE_SIZE);
        assert!(first.stat_filesystem().await.unwrap().block_size > 0);

        let options = FileOpenOptions {
            access: FileAccess::ReadWrite,
            truncate: false,
            append: false,
        };
        let created = first
            .create_file(ROOT_NODE, "clip.tmp".into(), 0o644, options)
            .await
            .unwrap();
        let node = created.metadata.node;
        let handle = created.opened.handle;
        let initial = first.write_range(handle, 0, b"0123456789").await.unwrap();
        assert_eq!(initial.written, 10);
        let overwritten = first.write_range(handle, 2, b"ABCD").await.unwrap();
        assert_eq!(overwritten.written, 4);
        let read = first.read_range_versioned(handle, 1, 7).await.unwrap();
        assert_eq!(read.data, b"1ABCD67");
        assert_eq!(read.revision, overwritten.revision);
        if capabilities.supports_preallocation {
            assert!(first.allocate_file(handle, 0, 4_096).await.unwrap().size >= 10);
        }
        first.sync_file(handle, false).await.unwrap();

        let first_lock = FileLock {
            owner: 11,
            start: 0,
            end: 1_023,
            kind: LockKind::Write,
            pid: 101,
        };
        first.set_lock(handle, first_lock, false).await.unwrap();

        let second = NetworkFilesystem::authenticate(
            server.pinned_client().await,
            "alice".into(),
            "correct horse battery staple".into(),
        )
        .await
        .unwrap();
        let second_handle = second
            .open_file_with_options(node, options)
            .await
            .unwrap()
            .handle;
        let requested_by_second = FileLock {
            owner: 22,
            pid: 202,
            ..first_lock
        };
        assert_eq!(
            second
                .get_lock(second_handle, requested_by_second)
                .await
                .unwrap(),
            Some(first_lock)
        );
        assert!(matches!(
            second
                .set_lock(second_handle, requested_by_second, false)
                .await,
            Err(ClientError::Server(ErrorCode::WouldBlock, _))
        ));

        // Dropping the first QUIC connection without closing its handle must
        // release its session-scoped locks before another client can proceed.
        drop(first);
        let mut acquired_after_disconnect = false;
        for _ in 0..40 {
            match second
                .set_lock(second_handle, requested_by_second, false)
                .await
            {
                Ok(()) => {
                    acquired_after_disconnect = true;
                    break;
                }
                Err(ClientError::Server(ErrorCode::WouldBlock, _)) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(error) => panic!("unexpected lock recovery error: {error}"),
            }
        }
        assert!(acquired_after_disconnect);
        second
            .set_lock(
                second_handle,
                FileLock {
                    kind: LockKind::Unlock,
                    ..requested_by_second
                },
                false,
            )
            .await
            .unwrap();

        let attributes = second
            .set_attributes(
                node,
                Some(second_handle),
                AttributeChanges {
                    size: Some(8),
                    mode: None,
                    accessed_unix_ms: None,
                    modified_unix_ms: None,
                    backup_unix_ms: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(attributes.size, 8);
        second.sync_file(second_handle, true).await.unwrap();
        second.close_file(second_handle).await.unwrap();

        let project = second
            .create_directory(ROOT_NODE, "project".into(), 0o755)
            .await
            .unwrap();
        second
            .rename_node(
                ROOT_NODE,
                "clip.tmp".into(),
                project.node,
                "clip.mov".into(),
                RenameMode::NoReplace,
            )
            .await
            .unwrap();
        assert_eq!(
            resolve_path(&second, "/project/clip.mov").await.unwrap(),
            node
        );
        second.sync_directory(project.node).await.unwrap();
        let link = second
            .create_symlink(ROOT_NODE, "clip-link".into(), b"project/clip.mov".to_vec())
            .await
            .unwrap();
        assert_eq!(
            second.read_link(link.node).await.unwrap(),
            b"project/clip.mov"
        );
        second
            .remove_node(ROOT_NODE, "clip-link".into(), false)
            .await
            .unwrap();
        second
            .remove_node(project.node, "clip.mov".into(), false)
            .await
            .unwrap();
        second
            .remove_node(ROOT_NODE, "project".into(), true)
            .await
            .unwrap();

        // The export-wide flag is necessary but not sufficient: a new login
        // after revocation must receive read-only capabilities and mutation
        // errors even while this daemon remains writable for granted users.
        set_user_writable(&server.state_path, "alice", false).unwrap();
        let revoked = NetworkFilesystem::authenticate(
            server.pinned_client().await,
            "alice".into(),
            "correct horse battery staple".into(),
        )
        .await
        .unwrap();
        assert!(!revoked.capabilities().await.unwrap().writable);
        assert!(matches!(
            revoked
                .create_directory(ROOT_NODE, "denied".into(), 0o755)
                .await,
            Err(ClientError::Server(ErrorCode::ReadOnly, _))
        ));
        drop(revoked);
        drop(second);
        server.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn v6_extended_filesystem_operations_cross_the_authenticated_transport() {
        let server = TestServer::start_writable(5_000).await;
        let filesystem = NetworkFilesystem::authenticate(
            server.pinned_client().await,
            "alice".into(),
            "correct horse battery staple".into(),
        )
        .await
        .unwrap();
        let capabilities = filesystem.capabilities().await.unwrap();
        assert!(capabilities.supports_xattrs);
        assert!(capabilities.supports_hard_links);
        assert!(capabilities.supports_copy_file_range);
        assert!(capabilities.supports_seek_data_hole);
        assert!(capabilities.supports_safe_ioctl);
        assert!(capabilities.supports_poll);
        assert!(capabilities.supports_bmap);
        assert!(capabilities.supports_exchange_data);
        assert!(capabilities.supports_volume_rename);
        assert!(capabilities.supports_backup_time);
        assert!(capabilities.supports_readdirplus);
        assert!(capabilities.persistent_node_ids);
        assert!(capabilities.restart_lock_replay);

        let options = FileOpenOptions {
            access: FileAccess::ReadWrite,
            truncate: false,
            append: false,
        };
        let left = filesystem
            .create_file(ROOT_NODE, "left.mov".into(), 0o600, options)
            .await
            .unwrap();
        let right = filesystem
            .create_file(ROOT_NODE, "right.mov".into(), 0o600, options)
            .await
            .unwrap();
        filesystem
            .write_range(left.opened.handle, 0, b"abcdefgh")
            .await
            .unwrap();
        filesystem
            .write_range(right.opened.handle, 0, b"1234")
            .await
            .unwrap();

        let tag = Name::from("com.apple.metadata:_kMDItemUserTags");
        filesystem
            .set_xattr(
                left.metadata.node,
                tag.clone(),
                b"test-tag",
                XattrSetMode::Create,
                0,
            )
            .await
            .unwrap();
        assert_eq!(
            filesystem
                .get_xattr(left.metadata.node, tag.clone(), 2, 4)
                .await
                .unwrap()
                .data,
            b"st-t"
        );
        assert!(
            filesystem
                .list_xattrs(left.metadata.node)
                .await
                .unwrap()
                .contains(&tag)
        );
        let view = filesystem
            .list_directory_view(ROOT_NODE, DirectoryViewOptions::NATIVE)
            .await
            .unwrap();
        let left_view = view
            .entries
            .iter()
            .find(|entry| entry.entry.node == left.metadata.node)
            .unwrap();
        let xattrs = left_view.xattrs.as_ref().unwrap();
        assert!(xattrs.names.contains(&tag));
        assert_eq!(
            xattrs
                .inline_values
                .iter()
                .find(|value| value.name == tag)
                .unwrap()
                .value,
            b"test-tag"
        );

        let resource_fork = Name::from("com.apple.ResourceFork");
        filesystem
            .set_xattr(
                left.metadata.node,
                resource_fork.clone(),
                b"left-fork",
                XattrSetMode::Create,
                0,
            )
            .await
            .unwrap();
        filesystem
            .set_xattr(
                right.metadata.node,
                resource_fork.clone(),
                b"right-fork",
                XattrSetMode::Create,
                0,
            )
            .await
            .unwrap();

        let linked = filesystem
            .create_hard_link(left.metadata.node, ROOT_NODE, "left-alias.mov".into())
            .await
            .unwrap();
        assert_eq!(linked.node, left.metadata.node);
        assert_eq!(linked.link_count, 2);
        let copied = filesystem
            .copy_file_range(left.opened.handle, 2, right.opened.handle, 1, 4)
            .await
            .unwrap();
        assert_eq!(copied.written, 4);
        assert_eq!(
            filesystem
                .seek_file(left.opened.handle, 0, SeekWhence::Data)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            filesystem
                .seek_file(left.opened.handle, 0, SeekWhence::Hole)
                .await
                .unwrap(),
            8
        );
        assert_eq!(
            filesystem
                .safe_ioctl(left.opened.handle, SafeIoctl::BytesAvailable)
                .await
                .unwrap(),
            8
        );
        assert_eq!(
            filesystem
                .map_block(left.metadata.node, 4_096, 0)
                .await
                .unwrap(),
            0
        );
        let backup = 1_700_555_000_123;
        assert_eq!(
            filesystem
                .set_attributes(
                    left.metadata.node,
                    Some(left.opened.handle),
                    AttributeChanges {
                        size: None,
                        mode: None,
                        accessed_unix_ms: None,
                        modified_unix_ms: None,
                        backup_unix_ms: Some(backup),
                    },
                )
                .await
                .unwrap()
                .backup_unix_ms,
            Some(backup)
        );

        filesystem
            .exchange_data(
                ROOT_NODE,
                "left.mov".into(),
                ROOT_NODE,
                "right.mov".into(),
                0,
            )
            .await
            .unwrap();
        assert_eq!(
            filesystem
                .read_range(left.opened.handle, 0, 16)
                .await
                .unwrap(),
            b"1cdef"
        );
        assert_eq!(
            filesystem
                .read_range(right.opened.handle, 0, 16)
                .await
                .unwrap(),
            b"abcdefgh"
        );
        assert_eq!(
            filesystem
                .get_xattr(left.metadata.node, resource_fork.clone(), 0, 32)
                .await
                .unwrap()
                .data,
            b"right-fork"
        );
        assert_eq!(
            filesystem
                .get_xattr(right.metadata.node, resource_fork.clone(), 0, 32)
                .await
                .unwrap()
                .data,
            b"left-fork"
        );

        filesystem
            .set_volume_name("Network Editing".into())
            .await
            .unwrap();
        assert_eq!(
            filesystem.capabilities().await.unwrap().volume_name,
            "Network Editing"
        );
        let listing = filesystem.list_directory_snapshot(ROOT_NODE).await.unwrap();
        assert!(
            listing
                .entries
                .iter()
                .all(|entry| entry.metadata.node == entry.node)
        );

        #[cfg(any(target_os = "linux", target_os = "android"))]
        if capabilities.supports_special_nodes {
            let fifo = filesystem
                .create_special_node(
                    ROOT_NODE,
                    "render.fifo".into(),
                    SpecialNodeKind::NamedPipe,
                    0o600,
                    0,
                    0,
                )
                .await
                .unwrap();
            assert_eq!(fifo.kind, NodeKind::NamedPipe);
            filesystem
                .remove_node(ROOT_NODE, "render.fifo".into(), false)
                .await
                .unwrap();

            let raw = Name::new(b"clip-\xff.mov".to_vec());
            let raw_directory = filesystem
                .create_directory(ROOT_NODE, raw.clone(), 0o700)
                .await
                .unwrap();
            assert!(
                filesystem
                    .list_directory(ROOT_NODE)
                    .await
                    .unwrap()
                    .iter()
                    .any(|entry| entry.name == raw)
            );
            filesystem.remove_node(ROOT_NODE, raw, true).await.unwrap();
            filesystem
                .forget_nodes(vec![fifo.node, raw_directory.node])
                .await
                .unwrap();
        }

        filesystem
            .remove_xattr(left.metadata.node, tag)
            .await
            .unwrap();
        filesystem.close_file(left.opened.handle).await.unwrap();
        filesystem.close_file(right.opened.handle).await.unwrap();
        filesystem
            .remove_node(ROOT_NODE, "left-alias.mov".into(), false)
            .await
            .unwrap();
        filesystem
            .remove_node(ROOT_NODE, "left.mov".into(), false)
            .await
            .unwrap();
        filesystem
            .remove_node(ROOT_NODE, "right.mov".into(), false)
            .await
            .unwrap();
        filesystem
            .forget_nodes(vec![left.metadata.node, right.metadata.node])
            .await
            .unwrap();
        drop(filesystem);
        server.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn persistent_epoch_node_and_volume_state_survive_full_daemon_restart() {
        let first_server = TestServer::start_writable(5_000).await;
        let first = NetworkFilesystem::authenticate(
            first_server.pinned_client().await,
            "alice".into(),
            "correct horse battery staple".into(),
        )
        .await
        .unwrap();
        let created = first
            .create_file(
                ROOT_NODE,
                "restart.mov".into(),
                0o600,
                FileOpenOptions {
                    access: FileAccess::ReadWrite,
                    truncate: false,
                    append: false,
                },
            )
            .await
            .unwrap();
        first
            .write_range(created.opened.handle, 0, b"restart-safe")
            .await
            .unwrap();
        first.sync_file(created.opened.handle, false).await.unwrap();
        first.close_file(created.opened.handle).await.unwrap();
        first
            .set_volume_name("Persistent Media".into())
            .await
            .unwrap();
        let node = created.metadata.node;
        let epoch = first.capabilities().await.unwrap().server_epoch;
        drop(first);
        let (state, export) = first_server.stop_preserving().await;

        let second_server =
            TestServer::start_prepared_with_write_setting(state, export, 5_000, true).await;
        let second = NetworkFilesystem::authenticate(
            second_server.pinned_client().await,
            "alice".into(),
            "correct horse battery staple".into(),
        )
        .await
        .unwrap();
        let capabilities = second.capabilities().await.unwrap();
        assert_eq!(capabilities.server_epoch, epoch);
        assert_eq!(capabilities.volume_name, "Persistent Media");
        assert_eq!(second.get_metadata(node).await.unwrap().node, node);
        assert_eq!(resolve_path(&second, "/restart.mov").await.unwrap(), node);
        let handle = second.open_file(node).await.unwrap().0;
        assert_eq!(
            second.read_range(handle, 0, 32).await.unwrap(),
            b"restart-safe"
        );
        second.close_file(handle).await.unwrap();
        drop(second);
        second_server.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_only_default_advertises_and_rejects_writes() {
        let server = TestServer::start(5_000).await;
        let filesystem = NetworkFilesystem::authenticate(
            server.pinned_client().await,
            "alice".into(),
            "correct horse battery staple".into(),
        )
        .await
        .unwrap();
        assert!(!filesystem.capabilities().await.unwrap().writable);
        let node = resolve_path(&filesystem, "/example.txt").await.unwrap();
        let (handle, _, _) = filesystem.open_file(node).await.unwrap();
        assert!(matches!(
            filesystem.write_range(handle, 0, b"mutate").await,
            Err(ClientError::Server(ErrorCode::ReadOnly, _))
        ));
        assert_eq!(
            filesystem.read_range(handle, 0, 22).await.unwrap(),
            b"authenticated contents"
        );
        filesystem.close_file(handle).await.unwrap();
        drop(filesystem);
        server.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enterprise_ca_authenticates_server_without_pairing() {
        let server_name = "files.enterprise.test";
        let (ca_pem, certificate_chain, private_key) = enterprise_identity(server_name);
        let server = TestServer::start_with_identity(
            5_000,
            certificate_chain.as_bytes(),
            private_key.as_bytes(),
        )
        .await;
        let authorities = parse_certificates_pem(ca_pem.as_bytes()).unwrap();

        assert!(
            QuicClient::connect_with_ca(
                server.address,
                "wrong.enterprise.test",
                authorities.clone(),
                Duration::from_secs(2),
            )
            .await
            .is_err()
        );
        let (untrusted_ca_pem, _, _) = enterprise_identity("untrusted.enterprise.test");
        assert!(
            QuicClient::connect_with_ca(
                server.address,
                server_name,
                parse_certificates_pem(untrusted_ca_pem.as_bytes()).unwrap(),
                Duration::from_secs(2),
            )
            .await
            .is_err()
        );

        let transport = QuicClient::connect_with_ca(
            server.address,
            server_name,
            authorities,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        let filesystem = NetworkFilesystem::authenticate(
            transport,
            "alice".into(),
            "correct horse battery staple".into(),
        )
        .await
        .unwrap();
        assert_eq!(filesystem.ping(101).await.unwrap(), 101);

        drop(filesystem);
        server.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn installed_identity_generation_is_presented_after_restart() {
        let server_name = "rotated.enterprise.test";
        let (ca_pem, certificate_chain, private_key) = enterprise_identity(server_name);
        let server = TestServer::start_with_installed_identity(
            5_000,
            certificate_chain.as_bytes(),
            private_key.as_bytes(),
        )
        .await;
        let authorities = parse_certificates_pem(ca_pem.as_bytes()).unwrap();
        let client = QuicClient::connect_with_ca(
            server.address,
            server_name,
            authorities,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(
            client.peer_certificate_fingerprint().unwrap(),
            server.fingerprint
        );
        client.close();
        server.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn incomplete_frames_are_timed_out() {
        let server = TestServer::start(100).await;
        let client = server.pinned_client().await;
        let (mut send, mut recv) = client.stream().await.unwrap();
        send.write_all(&100u32.to_be_bytes()).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(2), recv.read_to_end(1024))
                .await
                .is_ok()
        );
        let (mut oversized_send, mut oversized_recv) = client.stream().await.unwrap();
        oversized_send
            .write_all(&((MAX_FRAME_SIZE as u32) + 1).to_be_bytes())
            .await
            .unwrap();
        oversized_send.finish().unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(2), oversized_recv.read_to_end(1024))
                .await
                .is_ok()
        );
        assert!(matches!(
            request(&client, Request::Ping { nonce: 9 }).await,
            Response::Pong { nonce: 9 }
        ));
        drop(send);
        client.close();
        server.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn oversized_and_incomplete_write_bodies_are_bounded_and_timed_out() {
        let server = TestServer::start_writable(1_000).await;
        let client = server.pinned_client().await;
        assert!(matches!(
            request(
                &client,
                Request::Authenticate {
                    username: "alice".into(),
                    password: "correct horse battery staple".to_string().into(),
                },
            )
            .await,
            Response::AuthenticateAck
        ));
        let handle = match request(
            &client,
            Request::CreateFile {
                parent: ROOT_NODE,
                name: "upload.bin".into(),
                mode: 0o600,
                options: FileOpenOptions {
                    access: FileAccess::ReadWrite,
                    truncate: false,
                    append: false,
                },
            },
        )
        .await
        {
            Response::FileCreated { handle, .. } => handle,
            response => panic!("unexpected create response: {response:?}"),
        };
        assert!(matches!(
            request(
                &client,
                Request::WriteRange {
                    handle,
                    offset: 0,
                    length: DEFAULT_MAX_WRITE_SIZE + 1,
                },
            )
            .await,
            Response::Error(ProtocolError {
                code: ErrorCode::TooLarge,
                ..
            })
        ));

        let envelope = Envelope::new(Request::WriteRange {
            handle,
            offset: 0,
            length: 4,
        });
        let (mut send, mut recv) = client.stream().await.unwrap();
        client.send_frame(&mut send, &envelope).await.unwrap();
        send.write_all(b"x").await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(3), recv.read_to_end(1_024))
                .await
                .is_ok()
        );
        drop(send);
        assert!(matches!(
            request(&client, Request::Ping { nonce: 19 }).await,
            Response::Pong { nonce: 19 }
        ));
        client.close();
        server.stop().await;
    }

    #[tokio::test]
    async fn refuses_to_export_server_state() {
        let export = tempfile::tempdir().unwrap();
        let state = export.path().join(".quickfs");
        initialize(&state, vec!["localhost".into()]).unwrap();
        let configuration = test_configuration(&state, export.path(), 1_000);
        let result = serve_until(configuration, std::future::pending(), None).await;
        assert!(result.unwrap_err().to_string().contains("overlaps"));

        let pairings_export = state.join("pairings");
        let configuration = test_configuration(&state, &pairings_export, 1_000);
        let result = serve_until(configuration, std::future::pending(), None).await;
        assert!(result.unwrap_err().to_string().contains("overlaps"));
    }

    #[test]
    fn authentication_rate_limit_is_scoped_by_peer() {
        let first: IpAddr = "192.0.2.1".parse().unwrap();
        let second: IpAddr = "192.0.2.2".parse().unwrap();
        let mut limiter = AuthRateLimiter::new(2);
        assert!(limiter.allow(first));
        assert!(limiter.allow(first));
        assert!(!limiter.allow(first));
        assert!(limiter.allow(second));
    }

    #[test]
    fn external_identity_arguments_require_complete_exclusive_inputs() {
        assert!(Cli::try_parse_from(["server-daemon", "init"]).is_err());
        assert!(
            Cli::try_parse_from([
                "server-daemon",
                "init",
                "--certificate",
                "chain.pem",
                "--private-key",
                "key.pem"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "server-daemon",
                "init",
                "--server-name",
                "localhost",
                "--certificate",
                "chain.pem",
                "--private-key",
                "key.pem"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "server-daemon",
                "identity",
                "install",
                "--certificate",
                "chain.pem"
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["server-daemon", "user", "grant-write", "alice"]).is_ok());
        assert!(Cli::try_parse_from(["server-daemon", "user", "revoke-write", "alice"]).is_ok());
    }

    #[test]
    fn external_identity_validation_accepts_a_match_and_rejects_a_mismatch() {
        let directory = tempfile::tempdir().unwrap();
        let (_, certificate_chain, matching_private_key) =
            enterprise_identity("files.enterprise.test");
        let certificate_path = directory.path().join("chain.pem");
        let private_key_path = directory.path().join("key.pem");
        std::fs::write(&certificate_path, certificate_chain).unwrap();
        std::fs::write(&private_key_path, matching_private_key).unwrap();
        assert!(load_external_identity(&certificate_path, &private_key_path).is_ok());

        std::fs::write(
            &private_key_path,
            KeyPair::generate().unwrap().serialize_pem(),
        )
        .unwrap();
        assert!(load_external_identity(&certificate_path, &private_key_path).is_err());
    }
}
